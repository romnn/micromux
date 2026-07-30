#[cfg(target_os = "linux")]
use std::path::Path;

pub(crate) struct PortOwner {
    pub(crate) pid: u32,
    pub(crate) command: String,
}

#[cfg(target_os = "macos")]
const LSOF_PATH: &str = "/usr/sbin/lsof";

/// Whether a `/proc/net/tcp{,6}` hex local address is loopback or unspecified (wildcard).
///
/// The bind probe that triggers this lookup targets `127.0.0.1`, so only a loopback or
/// wildcard listener can actually be the holder; naming a listener bound to some other
/// interface would be a wrong claim.
#[cfg(target_os = "linux")]
fn is_loopback_or_unspecified(addr_hex: &str) -> bool {
    let upper = addr_hex.to_ascii_uppercase();
    match upper.len() {
        // IPv4, little-endian hex: 127.0.0.1 or 0.0.0.0.
        8 => upper == "00000000" || upper == "0100007F",
        // IPv6: `::`, `::1`, or an IPv4-mapped loopback/wildcard.
        32 => {
            upper.bytes().all(|byte| byte == b'0')
                || upper == "00000000000000000000000001000000"
                || upper.ends_with("FFFF00000100007F")
                || upper.ends_with("FFFF000000000000")
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn listening_inode(line: &str, port: u16) -> Option<u64> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.get(3).copied()? != "0A" {
        return None;
    }
    let local = fields.get(1).copied()?;
    let (encoded_addr, encoded_port) = local.rsplit_once(':')?;
    if !is_loopback_or_unspecified(encoded_addr) {
        return None;
    }
    (u16::from_str_radix(encoded_port, 16).ok()? == port).then(|| fields.get(9)?.parse().ok())?
}

#[cfg(target_os = "linux")]
fn socket_inode(port: u16) -> Option<u64> {
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|table| {
            table
                .lines()
                .filter_map(move |line| listening_inode(line, port))
                .collect::<Vec<_>>()
        })
        .next()
}

#[cfg(target_os = "linux")]
fn command_for_pid(pid_dir: &Path) -> String {
    let command = std::fs::read(pid_dir.join("cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|part| !part.is_empty())
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|command| !command.is_empty())
        .or_else(|| std::fs::read_to_string(pid_dir.join("comm")).ok())
        .unwrap_or_else(|| "unknown command".to_string());
    command.chars().take(120).collect()
}

#[cfg(target_os = "linux")]
pub(crate) fn listening_owner(port: u16) -> Option<PortOwner> {
    let inode = socket_inode(port)?;
    let target = format!("socket:[{inode}]");
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let pid_dir = entry.path();
        let Ok(fds) = std::fs::read_dir(pid_dir.join("fd")) else {
            continue;
        };
        if fds.flatten().any(|fd| {
            std::fs::read_link(fd.path())
                .ok()
                .is_some_and(|link| link == Path::new(&target))
        }) {
            return Some(PortOwner {
                pid,
                command: command_for_pid(&pid_dir),
            });
        }
    }
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn listening_owner(port: u16) -> Option<PortOwner> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new(LSOF_PATH)
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpc"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            // Reap on the error path too, or the lsof child lingers as a zombie.
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let pid = text
        .lines()
        .find_map(|line| line.strip_prefix('p'))?
        .parse()
        .ok()?;
    let command = text
        .lines()
        .find_map(|line| line.strip_prefix('c'))
        .unwrap_or("unknown command")
        .chars()
        .take(120)
        .collect();
    Some(PortOwner { pid, command })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn listening_owner(_port: u16) -> Option<PortOwner> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{listening_inode, listening_owner};
    use similar_asserts::assert_eq;

    #[test]
    fn parses_listening_proc_socket() {
        let line = "  0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000 0 123456 1 0000000000000000 100 0 0 10 0";
        assert_eq!(listening_inode(line, 8080), Some(123_456));
        assert_eq!(listening_inode(line, 8081), None);
        assert_eq!(
            listening_inode(&line.replacen(" 0A ", " 01 ", 1), 8080),
            None
        );
        // A listener bound to a specific non-loopback interface cannot hold 127.0.0.1.
        assert_eq!(
            listening_inode(&line.replacen("0100007F", "0101A8C0", 1), 8080),
            None
        );
        // A wildcard listener does.
        assert_eq!(
            listening_inode(&line.replacen("0100007F:1F90", "00000000:1F90", 1), 8080),
            Some(123_456)
        );
    }

    #[test]
    fn identifies_current_process_listener() -> std::io::Result<()> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let owner = listening_owner(port)
            .ok_or_else(|| std::io::Error::other("listener owner was not found"))?;
        assert_eq!(owner.pid, std::process::id());
        assert!(!owner.command.is_empty());
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::LSOF_PATH;
    use similar_asserts::assert_eq;
    use std::path::Path;

    #[test]
    fn lsof_uses_the_system_absolute_path() {
        assert!(Path::new(LSOF_PATH).is_absolute());
        assert_eq!(LSOF_PATH, "/usr/sbin/lsof");
    }
}
