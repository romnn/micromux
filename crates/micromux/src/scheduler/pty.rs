use super::{Event, LogUpdateKind, OutputStream};
use crate::{health_check, service::Service};
use color_eyre::eyre;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::thread;
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
    for (k, v) in env_vars.iter() {
        cmd.env(k, v);
    }

    let mut child = pair
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

    let pgid = pair.master.process_group_leader();
    let master = Arc::new(Mutex::new(pair.master));
    let writer = Arc::new(Mutex::new(writer));

    let _ = events_tx
        .send(Event::Started {
            service_id: service_id.clone(),
        })
        .await;

    thread::spawn({
        let events_tx = events_tx.clone();
        let service_id = service_id.clone();
        move || {
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

            if !interactive_logs {
                let mut reader = std::io::BufReader::new(reader);
                let mut line = String::new();
                loop {
                    line.clear();
                    let bytes = reader.read_line(&mut line)?;
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
            } else {
                use std::io::Read;

                let mut reader = reader;
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
                    let n = reader.read(&mut buf)?;
                    if n == 0 {
                        if pending_cr {
                            flush(LogUpdateKind::ReplaceLast, &mut line);
                        } else {
                            flush(LogUpdateKind::Append, &mut line);
                        }
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
            }
            Ok::<_, std::io::Error>(())
        }
    });

    tokio::spawn({
        let events_tx = events_tx.clone();
        let service_id = service_id.clone();
        let shutdown = shutdown.clone();
        let terminate = terminate.clone();
        let mut killer = killer;
        async move {
            let mut termination_started = false;
            let mut hard_killed = false;
            let mut kill_deadline: Option<tokio::time::Instant> = None;
            loop {
                tokio::select! {
                    _ = shutdown.cancelled(), if !termination_started => {
                        tracing::info!(pid, service_id, "killing process");
                        let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                        #[cfg(unix)]
                        {
                            if let Some(pgid) = pgid {
                                let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                            } else if let Some(pid) = pid {
                                let _ = nix::sys::signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                            }
                            kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = killer.kill();
                            hard_killed = true;
                        }
                        termination_started = true;
                    }
                    _ = terminate.cancelled(), if !termination_started => {
                        tracing::info!(pid, service_id, "killing process");
                        let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                        #[cfg(unix)]
                        {
                            if let Some(pgid) = pgid {
                                let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                            } else if let Some(pid) = pid {
                                let _ = nix::sys::signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                            }
                            kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = killer.kill();
                            hard_killed = true;
                        }
                        termination_started = true;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
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
                        let code = status.exit_code() as i32;
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
        }
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
                .await
            }
        });
    }

    Ok(PtyHandles { master, writer })
}
