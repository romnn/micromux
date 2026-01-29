use super::{Event, LogUpdateKind, OutputStream, ServiceID};
use crate::{health_check, service::Service};
use color_eyre::eyre;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;
#[cfg(unix)]
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use nix::sys::signal::Signal;

#[cfg(unix)]
use nix::unistd::Pid;

#[derive(Clone)]
pub(super) struct PtyHandles {
    pub(super) master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub(super) writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

fn env_vars_for_service(service: &Service) -> HashMap<String, String> {
    let mut env_vars: HashMap<String, String> = service
        .environment
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if service.enable_color {
        env_vars.insert("TERM".to_string(), "xterm-256color".to_string());
        env_vars.insert("CLICOLOR".to_string(), "1".to_string());
        env_vars.insert("CLICOLOR_FORCE".to_string(), "1".to_string());
        env_vars.insert("FORCE_COLOR".to_string(), "1".to_string());
    }

    env_vars
}

fn strip_clear_line_sequences(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.get(i) == Some(&0x1b) && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while matches!(bytes.get(j), Some(b) if b.is_ascii_digit()) {
                j += 1;
            }
            if bytes.get(j) == Some(&b'K') {
                i = j + 1;
                continue;
            }
        }

        if let Some(&b) = bytes.get(i) {
            out.push(b);
        }
        i += 1;
    }

    String::from_utf8_lossy(&out).to_string()
}

fn spawn_log_reader_thread(
    service_id: ServiceID,
    mut reader: Box<dyn std::io::Read + Send>,
    events_tx: mpsc::Sender<Event>,
    interactive_logs: bool,
) {
    thread::spawn(move || {
        if interactive_logs {
            let mut buf = [0u8; 4096];
            let mut line: Vec<u8> = Vec::new();
            let mut pending_cr = false;

            let flush = |update: LogUpdateKind, line: &mut Vec<u8>| {
                if line.is_empty() {
                    return;
                }
                let s = String::from_utf8_lossy(line).to_string();
                let s = strip_clear_line_sequences(&s);
                let _ = events_tx.blocking_send(Event::LogLine {
                    service_id: service_id.clone(),
                    stream: OutputStream::Stdout,
                    update,
                    line: s,
                });
                line.clear();
            };

            loop {
                let n = match reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(err) => return Err::<_, std::io::Error>(err),
                };
                if n == 0 {
                    flush(
                        if pending_cr {
                            LogUpdateKind::ReplaceLast
                        } else {
                            LogUpdateKind::Append
                        },
                        &mut line,
                    );
                    break;
                }

                let Some(slice) = buf.get(..n) else {
                    continue;
                };

                for &b in slice {
                    if pending_cr {
                        if b == b'\n' {
                            flush(LogUpdateKind::Append, &mut line);
                            pending_cr = false;
                            continue;
                        }
                        flush(LogUpdateKind::ReplaceLast, &mut line);
                        pending_cr = false;
                    }

                    match b {
                        b'\n' => {
                            flush(LogUpdateKind::Append, &mut line);
                        }
                        b'\r' => {
                            pending_cr = true;
                        }
                        _ => {
                            line.push(b);
                        }
                    }
                }
            }
        } else {
            let mut reader = std::io::BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let bytes = match reader.read_line(&mut line) {
                    Ok(bytes) => bytes,
                    Err(err) => return Err::<_, std::io::Error>(err),
                };
                if bytes == 0 {
                    break;
                }

                while line.ends_with(['\n', '\r']) {
                    line.pop();
                }

                let line = strip_clear_line_sequences(&line);
                let _ = events_tx.blocking_send(Event::LogLine {
                    service_id: service_id.clone(),
                    stream: OutputStream::Stdout,
                    update: LogUpdateKind::Append,
                    line,
                });
            }
        }

        Ok::<_, std::io::Error>(())
    });
}

struct TerminationTaskArgs {
    service_id: ServiceID,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    process_group_leader_id: Option<i32>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

fn spawn_termination_task(args: TerminationTaskArgs) {
    tokio::spawn(async move {
        let TerminationTaskArgs {
            service_id,
            events_tx,
            shutdown,
            terminate,
            mut killer,
            pid,
            process_group_leader_id,
            mut child,
        } = args;

        let mut termination_started = false;
        let mut hard_killed = false;
        #[cfg(unix)]
        let mut kill_deadline: Option<tokio::time::Instant> = None;
        #[cfg(not(unix))]
        let kill_deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                () = shutdown.cancelled(), if !termination_started => {
                    tracing::info!(pid, service_id, "killing process");
                    let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = process_group_leader_id {
                            let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                        } else if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM);
                        }
                        kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = process_group_leader_id;
                        let _ = killer.kill();
                        hard_killed = true;
                    }
                    termination_started = true;
                }
                () = terminate.cancelled(), if !termination_started => {
                    tracing::info!(pid, service_id, "killing process");
                    let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = process_group_leader_id {
                            let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                        } else if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM);
                        }
                        kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = process_group_leader_id;
                        let _ = killer.kill();
                        hard_killed = true;
                    }
                    termination_started = true;
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }

            if termination_started
                && !hard_killed
                && let Some(deadline) = kill_deadline
                && tokio::time::Instant::now() >= deadline
            {
                let _ = killer.kill();
                hard_killed = true;
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    let _ = events_tx
                        .send(Event::Exited(service_id.clone(), code))
                        .await;
                    break;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::error!(?err, "failed to poll process status");
                    let _ = events_tx.send(Event::Exited(service_id.clone(), -1)).await;
                    break;
                }
            }
        }
    });
}

pub(super) async fn start_service_with_pty_size(
    service: &Service,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
    pty_size: portable_pty::PtySize,
    interactive_logs: bool,
) -> eyre::Result<PtyHandles> {
    use portable_pty::{CommandBuilder, PtySize};

    let service_id = service.id.clone();
    let (prog, args) = &service.command;

    let env_vars = env_vars_for_service(service);

    tracing::info!(service_id, prog, ?args, ?env_vars, "start service");

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: pty_size.rows,
            cols: pty_size.cols,
            pixel_width: pty_size.pixel_width,
            pixel_height: pty_size.pixel_height,
        })
        .map_err(|err| eyre::eyre!("failed to open pty: {err}"))?;

    let mut cmd = CommandBuilder::new(prog);
    cmd.args(args);
    if let Some(dir) = &service.working_dir {
        cmd.cwd(dir);
    }
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| eyre::eyre!("failed to spawn in pty: {err}"))?;

    let pid = child.process_id();
    let killer = child.clone_killer();

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| eyre::eyre!("failed to clone pty reader: {err}"))?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|err| eyre::eyre!("failed to take pty writer: {err}"))?;

    #[cfg(unix)]
    let process_group_leader = pair.master.process_group_leader();
    #[cfg(not(unix))]
    let process_group_leader = None;
    let master = Arc::new(Mutex::new(pair.master));
    let writer = Arc::new(Mutex::new(writer));

    let _ = events_tx
        .send(Event::Started {
            service_id: service_id.clone(),
        })
        .await;

    spawn_log_reader_thread(
        service_id.clone(),
        reader,
        events_tx.clone(),
        interactive_logs,
    );

    spawn_termination_task(TerminationTaskArgs {
        service_id: service_id.clone(),
        events_tx: events_tx.clone(),
        shutdown: shutdown.clone(),
        terminate: terminate.clone(),
        killer,
        pid,
        process_group_leader_id: process_group_leader,
        child,
    });

    if let Some(health_check) = service.health_check.clone() {
        tokio::spawn({
            let service_id = service_id.clone();
            let working_dir = service.working_dir.clone();
            let environment: std::collections::HashMap<String, String> = service
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let events_tx = events_tx.clone();
            let shutdown = shutdown.clone();
            let terminate = terminate.clone();
            async move {
                health_check::run_loop(
                    health_check,
                    &service_id,
                    working_dir,
                    environment,
                    events_tx,
                    shutdown,
                    terminate,
                )
                .await;
            }
        });
    }

    Ok(PtyHandles { master, writer })
}
