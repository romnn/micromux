use crate::{
    ServiceMap,
    graph::ServiceGraph,
    health_check::Health,
    service::{self, Service},
};
use color_eyre::eyre;
use futures::{FutureExt, SinkExt, channel::oneshot::Cancellation};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use nix::sys::signal::Signal;

#[cfg(unix)]
use nix::unistd::Pid;

pub type ServiceID = String;

#[derive(Debug, enum_as_inner::EnumAsInner)]
pub enum State {
    /// Service has not yet started.
    // #[strum(serialize = "PENDING")]
    Pending,
    Starting,
    /// Service is running.
    // #[strum(serialize = "RUNNING")]
    Running {
        // process: async_process::Child,
        health: Option<Health>,
    },
    /// Service is disabled.
    // #[strum(serialize = "DISABLED")]
    Disabled,
    /// Service exited with code.
    // #[strum(serialize = "EXITED")]
    Exited {
        exit_code: i32,
    },
    /// Service has been killed and is awaiting exit
    // #[strum(serialize = "KILLED")]
    Killed,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Starting => write!(f, "Starting"),
            Self::Running { health: None } => write!(f, "Running"),
            Self::Running {
                health: Some(health),
            } => write!(f, "Running({health})"),
            Self::Disabled => write!(f, "Disabled"),
            Self::Exited { exit_code } => write!(f, "Exited({exit_code})"),
            Self::Killed => write!(f, "Killed"),
        }
    }
}

fn update_state(
    services: &ServiceMap,
    service_state: &mut HashMap<ServiceID, State>,
    event: &Event,
) {
    match event {
        Event::Started { service_id } => {
            service_state.insert(service_id.clone(), State::Running { health: None });
        }
        Event::Healthy(service_id) => {
            service_state.insert(
                service_id.clone(),
                State::Running {
                    health: Some(Health::Healthy),
                },
            );
        }
        Event::Unhealthy(service_id) => {
            service_state.insert(
                service_id.clone(),
                State::Running {
                    health: Some(Health::Unhealthy),
                },
            );
        }
        Event::Killed(service_id) => {
            service_state.insert(service_id.clone(), State::Killed);
        }
        Event::Exited(service_id, exit_code) => {
            service_state.insert(
                service_id.clone(),
                State::Exited {
                    exit_code: *exit_code,
                },
            );
        }
        Event::Disabled(service_id) => {
            service_state.insert(service_id.clone(), State::Disabled);
        }
        Event::LogLine { .. }
        | Event::HealthCheckStarted { .. }
        | Event::HealthCheckLogLine { .. }
        | Event::HealthCheckFinished { .. } => {}
    }

    for service_id in services.keys() {
        service_state
            .entry(service_id.clone())
            .or_insert(State::Pending);
    }
}

#[derive(Debug)]
pub enum Event {
    Started {
        service_id: ServiceID,
    },
    LogLine {
        service_id: ServiceID,
        stream: OutputStream,
        update: LogUpdateKind,
        line: String,
    },
    HealthCheckStarted {
        service_id: ServiceID,
        attempt: u64,
        command: String,
    },
    HealthCheckLogLine {
        service_id: ServiceID,
        attempt: u64,
        stream: OutputStream,
        line: String,
    },
    HealthCheckFinished {
        service_id: ServiceID,
        attempt: u64,
        success: bool,
        exit_code: i32,
    },
    Killed(ServiceID),
    Exited(ServiceID, i32),
    Healthy(ServiceID),
    Unhealthy(ServiceID),
    Disabled(ServiceID),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogUpdateKind {
    Append,
    ReplaceLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl Event {
    pub fn service_id(&self) -> &ServiceID {
        match self {
            Self::Started { service_id, .. } => service_id,
            Self::LogLine { service_id, .. } => service_id,
            Self::HealthCheckStarted { service_id, .. } => service_id,
            Self::HealthCheckLogLine { service_id, .. } => service_id,
            Self::HealthCheckFinished { service_id, .. } => service_id,
            Self::Killed(service_id) => service_id,
            Self::Exited(service_id, _) => service_id,
            Self::Healthy(service_id) => service_id,
            Self::Unhealthy(service_id) => service_id,
            Self::Disabled(service_id) => service_id,
        }
    }
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started { service_id, .. } => write!(f, "Started({service_id})"),
            Self::LogLine {
                service_id, stream, ..
            } => write!(f, "LogLine({service_id}, {stream:?})"),
            Self::HealthCheckStarted {
                service_id,
                attempt,
                ..
            } => write!(f, "HealthCheckStarted({service_id}, attempt={attempt})"),
            Self::HealthCheckLogLine {
                service_id,
                stream,
                attempt,
                ..
            } => write!(
                f,
                "HealthCheckLogLine({service_id}, {stream:?}, attempt={attempt})"
            ),
            Self::HealthCheckFinished {
                service_id,
                attempt,
                success,
                exit_code,
            } => write!(
                f,
                "HealthCheckFinished({service_id}, attempt={attempt}, success={success}, exit_code={exit_code})"
            ),
            Self::Killed(service_id) => write!(f, "Killed({service_id})"),
            Self::Exited(service_id, _) => write!(f, "Exited({service_id})"),
            Self::Healthy(service_id) => write!(f, "Healthy({service_id})"),
            Self::Unhealthy(service_id) => write!(f, "Unhealthy({service_id})"),
            Self::Disabled(service_id) => write!(f, "Disabled({service_id})"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Restart(ServiceID),
    RestartAll,
    Disable(ServiceID),
    Enable(ServiceID),
    SendInput(ServiceID, Vec<u8>),
    ResizeAll { cols: u16, rows: u16 },
}

// shutdown: crate::shutdown::Shutdown,
// mut shutdown_handle: crate::shutdown::Handle,
// let args: Vec<String> = shlex::split(&service.command).unwrap_or_default();
// let Some((program, program_args)) = args.split_first() else {
//     eyre::bail!("bad command: {:?}", service.command);
// };

/// Start service.
async fn start_service(
    service: &Service,
    events_tx: mpsc::Sender<Event>,

    shutdown: CancellationToken,
    terminate: CancellationToken,
) -> eyre::Result<PtyHandles> {
    start_service_with_pty_size(
        service,
        events_tx,
        shutdown,
        terminate,
        portable_pty::PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        },
        true,
    )
    .await
}

async fn start_service_with_pty_size(
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
        env_vars.insert("CLICOLOR".to_string(), "1".to_string()); // many UNIX tools look at this
        env_vars.insert("CLICOLOR_FORCE".to_string(), "1".to_string()); // many UNIX tools look at this
        env_vars.insert("FORCE_COLOR".to_string(), "1".to_string()); // used by e.g. npm, chalk, etc.
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
                // Remove ANSI "erase in line" sequences (CSI n K). These are commonly
                // used together with carriage returns for progress displays.
                let bytes = s.as_bytes();
                let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
                let mut i = 0;
                while i < bytes.len() {
                    if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
                        let mut j = i + 2;
                        while j < bytes.len() && bytes[j].is_ascii_digit() {
                            j += 1;
                        }
                        if j < bytes.len() && bytes[j] == b'K' {
                            // skip ESC [ <digits> K
                            i = j + 1;
                            continue;
                        }
                    }

                    out.push(bytes[i]);
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

                let mut flush = |update: LogUpdateKind, line: &mut Vec<u8>| {
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

                    for &b in &buf[..n] {
                        if pending_cr {
                            if b == b'\n' {
                                // Windows-style CRLF newline.
                                flush(LogUpdateKind::Append, &mut line);
                                pending_cr = false;
                                continue;
                            }
                            // Bare CR (progress update boundary)
                            flush(LogUpdateKind::ReplaceLast, &mut line);
                            pending_cr = false;
                        }

                        match b {
                            b'\n' => {
                                flush(LogUpdateKind::Append, &mut line);
                            }
                            b'\r' => {
                                // Defer flush until we know whether this is CRLF (newline) or bare CR.
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

    // Monitor for exit or shutdown
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
                            // Try to terminate gracefully first.
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

                if termination_started && !hard_killed {
                    if let Some(deadline) = kill_deadline {
                        if tokio::time::Instant::now() >= deadline {
                            let _ = killer.kill();
                            hard_killed = true;
                        }
                    }
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

    // If the service specifies a health check, start the health check loop
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
                health_check
                    .run_loop(
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

#[derive(Clone)]
struct PtyHandles {
    master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

async fn schedule_ready(
    services: &ServiceMap,
    graph: &petgraph::graphmap::DiGraphMap<&str, ()>,
    service_state: &mut HashMap<ServiceID, State>,
    desired_disabled: &HashSet<ServiceID>,
    restart_requested: &mut HashSet<ServiceID>,
    restart_on_failure_remaining: &mut HashMap<ServiceID, usize>,
    terminate_tokens: &mut HashMap<ServiceID, CancellationToken>,
    pty_masters: &mut HashMap<ServiceID, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    pty_writers: &mut HashMap<ServiceID, Arc<Mutex<Box<dyn Write + Send>>>>,
    current_pty_size: portable_pty::PtySize,
    restart_backoff_until: &HashMap<ServiceID, tokio::time::Instant>,
    interactive_logs: bool,
    // events_rx: &mpsc::Receiver<Event>,
    events_tx: &mpsc::Sender<Event>,
    ui_tx: &mpsc::Sender<Event>,
    // broadcast_tx: &broadcast::Sender<Event>,
    // shutdown_handle: &crate::shutdown::Handle,
    shutdown: &CancellationToken,
) {
    use crate::{config::DependencyCondition, service::RestartPolicy};

    // Find services that are ready to start
    for (service_id, service) in services {
        if desired_disabled.contains(service_id) {
            continue;
        }

        if !restart_requested.contains(service_id) {
            if let Some(until) = restart_backoff_until.get(service_id) {
                if tokio::time::Instant::now() < *until {
                    continue;
                }
            }
        }
        // Compute scheduling decision using an immutable view first.
        let (should_consider_start, exited_code) = match service_state.get(service_id.as_str()) {
            None => continue,
            Some(state) => match state {
                State::Pending => {
                    // Proceed to check if service is ready to be started
                    (true, None)
                }
                State::Starting | State::Running { .. } | State::Killed | State::Disabled => {
                    // Skip disabled or already running service
                    // Killed processes will eventually exit and become ready for restart.
                    (false, None)
                }
                State::Exited { exit_code } => {
                    if restart_requested.contains(service_id) {
                        (true, Some(*exit_code))
                    } else {
                        match &services[service_id].restart_policy {
                            RestartPolicy::Never => (false, Some(*exit_code)),
                            RestartPolicy::Always | RestartPolicy::UnlessStopped => {
                                (true, Some(*exit_code))
                            }
                            RestartPolicy::OnFailure { remaining_attempts } => {
                                if *exit_code == 0 {
                                    (false, Some(*exit_code))
                                } else {
                                    let remaining = restart_on_failure_remaining
                                        .entry(service_id.clone())
                                        .or_insert(*remaining_attempts);
                                    (*remaining > 0, Some(*exit_code))
                                }
                            }
                        }
                    }
                }
            },
        };

        if !should_consider_start {
            continue;
        }

        tracing::debug!(
            service_id,
            state = ?service_state.get(service_id.as_str()),
            // state = ?service.state,
            "evaluating service"
        );

        // Only start if not already Running/Healthy
        // if matches!(state, State::Pending | State::Exited { .. }) {
        // Find dependencies
        let mut dependencies = graph.neighbors_directed(service_id, petgraph::Incoming);

        // All dependencies must be ready
        let is_ready = dependencies.all(|dep| {
            let condition = service
                .depends_on
                .iter()
                .find(|dep_config| dep_config.name.as_ref() == dep)
                .and_then(|dep_config| dep_config.condition.as_ref())
                .map(|condition| condition.as_ref())
                .copied()
                .unwrap_or_default();
            let state = match service_state.get(dep) {
                Some(state) => state,
                None => return false,
            };
            let is_ready = match condition {
                DependencyCondition::ServiceStarted => {
                    matches!(state, State::Running { .. })
                }
                DependencyCondition::ServiceHealthy => {
                    matches!(
                        state,
                        State::Running {
                            health: Some(Health::Healthy),
                            ..
                        }
                    )
                }
                DependencyCondition::ServiceCompletedSuccessfully => {
                    matches!(state, State::Exited { exit_code: 0, .. })
                }
            };
            is_ready
        });

        if is_ready {
            // Start service
            tracing::info!(service_id, "starting service");
            if let Some(exit_code) = exited_code {
                let policy = &services[service_id].restart_policy;
                if matches!(policy, RestartPolicy::OnFailure { .. })
                    && !restart_requested.contains(service_id)
                    && exit_code != 0
                {
                    if let Some(remaining) = restart_on_failure_remaining.get_mut(service_id) {
                        *remaining = remaining.saturating_sub(1);
                    }
                }
            }

            restart_requested.remove(service_id);

            if let Some(state) = service_state.get_mut(service_id.as_str()) {
                *state = State::Starting;
            }

            let terminate = CancellationToken::new();
            terminate_tokens.insert(service_id.clone(), terminate.clone());
            match start_service_with_pty_size(
                service,
                events_tx.clone(),
                shutdown.clone(),
                terminate.clone(),
                current_pty_size,
                interactive_logs,
            )
            .await
            {
                Ok(handles) => {
                    pty_masters.insert(service_id.clone(), handles.master);
                    pty_writers.insert(service_id.clone(), handles.writer);
                }
                Err(err) => {
                    tracing::error!(?err, service_id, "failed to start service");
                    if let Some(state) = service_state.get_mut(service_id.as_str()) {
                        *state = State::Exited { exit_code: -1 };
                    }
                }
            }
        }
    }
}

pub async fn scheduler(
    services: &ServiceMap,
    mut commands_rx: mpsc::Receiver<Command>,
    mut events_rx: mpsc::Receiver<Event>,
    mut events_tx: mpsc::Sender<Event>,
    mut ui_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    interactive_logs: bool,
) -> eyre::Result<()> {
    let graph = ServiceGraph::new(&services)?;

    let mut desired_disabled: HashSet<ServiceID> = HashSet::new();
    let mut restart_requested: HashSet<ServiceID> = HashSet::new();
    let mut terminate_tokens: HashMap<ServiceID, CancellationToken> = HashMap::new();
    let mut pty_masters: HashMap<ServiceID, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>> =
        HashMap::new();
    let mut pty_writers: HashMap<ServiceID, Arc<Mutex<Box<dyn Write + Send>>>> = HashMap::new();
    let mut current_pty_size = portable_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut restart_backoff_until: HashMap<ServiceID, tokio::time::Instant> = HashMap::new();
    let mut restart_backoff_delay: HashMap<ServiceID, Duration> = HashMap::new();
    let mut restart_on_failure_remaining: HashMap<ServiceID, usize> = services
        .iter()
        .filter_map(|(service_id, service)| match service.restart_policy {
            service::RestartPolicy::OnFailure { remaining_attempts } => {
                Some((service_id.clone(), remaining_attempts))
            }
            _ => None,
        })
        .collect();

    // Initially, all services are in pending state
    let mut service_state: HashMap<ServiceID, State> = services
        .keys()
        .map(|service_id| (service_id.to_string(), State::Pending))
        .collect();

    // Initial scheduling pass
    tracing::debug!("started initial scheduling pass");
    schedule_ready(
        &services,
        &graph.inner,
        &mut service_state,
        &desired_disabled,
        &mut restart_requested,
        &mut restart_on_failure_remaining,
        &mut terminate_tokens,
        &mut pty_masters,
        &mut pty_writers,
        current_pty_size,
        &restart_backoff_until,
        interactive_logs,
        &events_tx,
        &ui_tx,
        &shutdown,
    )
    .await;
    tracing::debug!("completed initial scheduling pass");

    // let handle_command = |service_state: &mut HashMap<String, State>, command: Command| {
    //     tracing::debug!(?command, "received command");
    //     match command {
    //         Command::Restart(service_id) => {
    //             tracing::debug!(service_id, "TODO: restart service");
    //             service_state.get(&service_id).unwrap();
    //         }
    //         Command::Disable(service_id) => {
    //             tracing::debug!(service_id, "TODO: disable service");
    //             let service = service_state.get(&service_id).unwrap();
    //             service.
    //         }
    //     }
    //     Ok::<_, eyre::Report>(())
    // };

    // let handle_event = |service_state: &mut HashMap<String, State>, event: Event| {
    //     tracing::debug!(%event, "received event");
    //
    //     update_state(services, service_state, &event);
    //
    //     // Forward event to the UI
    //     ui_tx.send(event).await?;
    //     Ok::<_, eyre::Report>(())
    // };

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    // let mut rounds_left: usize = 3;
    loop {
        tracing::debug!("waiting for scheduling event");
        // if rounds_left <= 0 {
        //     break;
        // }
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!("exiting scheduler");
                break;
            }
            command = commands_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                match command {
                    Command::Restart(service_id) => {
                        desired_disabled.remove(&service_id);
                        restart_requested.insert(service_id.clone());
                        restart_backoff_until.remove(&service_id);
                        restart_backoff_delay.remove(&service_id);
                        if let Some(terminate) = terminate_tokens.get(&service_id) {
                            terminate.cancel();
                        }
                    }
                    Command::RestartAll => {
                        for service_id in services.keys() {
                            desired_disabled.remove(service_id);
                            restart_requested.insert(service_id.clone());
                            restart_backoff_until.remove(service_id);
                            restart_backoff_delay.remove(service_id);
                            if let Some(terminate) = terminate_tokens.get(service_id) {
                                terminate.cancel();
                            }
                        }
                    }
                    Command::Disable(service_id) => {
                        desired_disabled.insert(service_id.clone());
                        let _ = ui_tx.send(Event::Disabled(service_id.clone())).await;
                        restart_backoff_until.remove(&service_id);
                        restart_backoff_delay.remove(&service_id);
                        if let Some(terminate) = terminate_tokens.get(&service_id) {
                            terminate.cancel();
                        }
                    }
                    Command::Enable(service_id) => {
                        desired_disabled.remove(&service_id);
                        restart_requested.insert(service_id.clone());
                        restart_backoff_until.remove(&service_id);
                        restart_backoff_delay.remove(&service_id);
                    }
                    Command::SendInput(service_id, data) => {
                        let Some(writer) = pty_writers.get(&service_id) else {
                            continue;
                        };
                        let mut guard = match writer.lock() {
                            Ok(guard) => guard,
                            Err(_) => {
                                tracing::warn!(service_id, "pty writer lock poisoned");
                                continue;
                            }
                        };
                        if let Err(err) = guard.write_all(&data) {
                            tracing::warn!(?err, service_id, "failed to write to pty");
                        }
                        if let Err(err) = guard.flush() {
                            tracing::warn!(?err, service_id, "failed to flush pty");
                        }
                    }
                    Command::ResizeAll { cols, rows } => {
                        current_pty_size = portable_pty::PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        };
                        for (service_id, master) in pty_masters.iter() {
                            let guard = match master.lock() {
                                Ok(guard) => guard,
                                Err(_) => {
                                    tracing::warn!(service_id, "pty master lock poisoned");
                                    continue;
                                }
                            };
                            let res = guard.resize(portable_pty::PtySize {
                                rows,
                                cols,
                                pixel_width: 0,
                                pixel_height: 0,
                            });
                            if let Err(err) = res {
                                tracing::warn!(?err, service_id, "failed to resize pty");
                            }
                        }
                    }
                }
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                // handle_event(&mut service_state, event)?;
                tracing::debug!(%event, "received event");

                let service_id = event.service_id().clone();
                update_state(services, &mut service_state, &event);

                if matches!(event, Event::Exited(_, _)) {
                    terminate_tokens.remove(&service_id);
                    pty_masters.remove(&service_id);
                    pty_writers.remove(&service_id);

                    // Basic restart backoff for crash loops.
                    if let Event::Exited(_, code) = &event {
                        if *code != 0 && !desired_disabled.contains(&service_id) {
                            let delay = restart_backoff_delay
                                .entry(service_id.clone())
                                .and_modify(|d| {
                                    *d = (*d * 2).min(Duration::from_secs(10));
                                })
                                .or_insert(Duration::from_millis(250));
                            restart_backoff_until.insert(
                                service_id.clone(),
                                tokio::time::Instant::now() + *delay,
                            );
                        } else {
                            restart_backoff_until.remove(&service_id);
                            restart_backoff_delay.remove(&service_id);
                        }
                    }
                }

                // Forward event to the UI
                ui_tx.send(event).await?;
            }
        }

        schedule_ready(
            &services,
            &graph.inner,
            &mut service_state,
            &desired_disabled,
            &mut restart_requested,
            &mut restart_on_failure_remaining,
            &mut terminate_tokens,
            &mut pty_masters,
            &mut pty_writers,
            current_pty_size,
            &restart_backoff_until,
            interactive_logs,
            &events_tx,
            &ui_tx,
            &shutdown,
        )
        .await;
        // rounds_left = rounds_left.saturating_sub(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::service::Service;
    use color_eyre::eyre;
    use indexmap::IndexMap;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{Duration, timeout};
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: Default::default(),
            inner: value.to_string(),
        }
    }

    fn service_config(name: &str, command: (&str, &[&str])) -> config::Service {
        config::Service {
            name: spanned_string(name),
            command: (
                spanned_string(command.0),
                command
                    .1
                    .iter()
                    .map(|v| spanned_string(v))
                    .collect::<Vec<_>>(),
            ),
            working_dir: None,
            env_file: vec![],
            environment: IndexMap::new(),
            depends_on: vec![],
            healthcheck: None,
            ports: vec![],
            restart: None,
            color: None,
        }
    }

    fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
    }

    async fn recv_event(
        mut rx: mpsc::Receiver<Event>,
    ) -> eyre::Result<(Event, mpsc::Receiver<Event>)> {
        let ev = timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| eyre::eyre!("timeout waiting for event"))?
            .ok_or_else(|| eyre::eyre!("event channel closed"))?;
        Ok((ev, rx))
    }

    fn healthcheck_always_ok() -> config::HealthCheck {
        config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("exit 0")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: Default::default(),
                inner: 10,
            }),
        }
    }

    #[tokio::test]
    async fn healthcheck_inherits_environment() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.environment
            .insert(spanned_string("HC_FOO"), spanned_string("bar"));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![
                    spanned_string("-c"),
                    spanned_string("[ \"$HC_FOO\" = \"bar\" ]"),
                ],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: Default::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let mut saw_hc_success = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if let Event::HealthCheckFinished { success: true, .. } = event {
                saw_hc_success = true;
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_hc_success);
        Ok(())
    }

    #[tokio::test]
    async fn healthcheck_inherits_working_dir() -> eyre::Result<()> {
        let dir = unique_tmp_dir("healthcheck-cwd");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("marker.txt"), "ok")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.working_dir = Some(spanned_string(dir.to_string_lossy().as_ref()));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("test -f marker.txt")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: Default::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", &dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let mut saw_hc_success = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if let Event::HealthCheckFinished { success: true, .. } = event {
                saw_hc_success = true;
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_hc_success);
        Ok(())
    }

    #[tokio::test]
    async fn healthcheck_spawn_error_emits_log_line() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("definitely-not-a-real-binary"),
                vec![spanned_string("--version")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: Default::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: Default::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let mut saw_log_line = false;
        let mut saw_finished = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::HealthCheckLogLine { stream, line, .. }
                    if matches!(stream, OutputStream::Stderr) && !line.is_empty() =>
                {
                    saw_log_line = true;
                }
                Event::HealthCheckFinished {
                    success: false,
                    exit_code: -1,
                    ..
                } => {
                    saw_finished = true;
                    if saw_log_line {
                        break;
                    }
                }
                _ => {}
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_log_line);
        assert!(saw_finished);
        Ok(())
    }

    #[tokio::test]
    async fn disable_kills_running_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        commands_tx
            .send(Command::Disable("svc".to_string()))
            .await?;

        loop {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Killed(_) | Event::Exited(_, _)) {
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn restart_restarts_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        commands_tx
            .send(Command::Restart("svc".to_string()))
            .await?;

        let mut saw_second_start = false;
        for _ in 0..10 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Started { .. }) {
                saw_second_start = true;
                break;
            }
        }

        assert!(saw_second_start);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn emits_log_lines() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "echo hello && echo err >&2"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_hello = false;
        let mut saw_err = false;

        for _ in 0..50 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("hello") => {
                    saw_hello = true;
                }
                Event::LogLine { line, .. } if line.contains("err") => {
                    saw_err = true;
                }
                Event::Exited(_, _) if saw_hello && saw_err => break,
                _ => {}
            }
        }

        assert!(saw_hello);
        assert!(saw_err);

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn child_sees_tty() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config(
                    "svc",
                    (
                        "sh",
                        &["-c", "if tty -s; then echo tty; else echo notty; fi"],
                    ),
                ),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_tty = false;
        let mut saw_exit = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("tty") => {
                    saw_tty = true;
                }
                Event::Exited(_, _) => {
                    saw_exit = true;
                    if saw_tty {
                        break;
                    }
                }
                _ => {}
            }
        }

        assert!(saw_tty);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn send_input_reaches_process() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "read line; echo got:$line"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        commands_tx
            .send(Command::SendInput("svc".to_string(), b"hello\r".to_vec()))
            .await?;

        let mut saw = false;
        for _ in 0..30 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("got:hello") => {
                    saw = true;
                }
                Event::Exited(_, _) => {
                    if saw {
                        break;
                    }
                    return Err(eyre::eyre!("process exited before receiving input"));
                }
                _ => {}
            }
        }

        assert!(saw);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn depends_on_service_healthy_delays_start() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();

        let mut dep_cfg = service_config("dep", ("sh", &["-c", "sleep 60"]));
        dep_cfg.healthcheck = Some(healthcheck_always_ok());
        services.insert("dep".to_string(), Service::new("dep", config_dir, dep_cfg)?);

        let mut app_cfg = service_config("app", ("sh", &["-c", "sleep 60"]));
        app_cfg.depends_on = vec![config::Dependency {
            name: spanned_string("dep"),
            condition: Some(Spanned {
                span: Default::default(),
                inner: config::DependencyCondition::ServiceHealthy,
            }),
        }];
        services.insert("app".to_string(), Service::new("app", config_dir, app_cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (_commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { service_id } if service_id == "dep"));

        let mut saw_app_started = false;
        let mut saw_dep_healthy = false;
        for _ in 0..40 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::Healthy(service_id) if service_id == "dep" => {
                    saw_dep_healthy = true;
                }
                Event::Started { service_id } if service_id == "app" => {
                    saw_app_started = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(saw_dep_healthy);
        assert!(saw_app_started);

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn resize_all_changes_stty_size_for_new_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();

        services.insert(
            "dep".to_string(),
            Service::new(
                "dep",
                config_dir,
                service_config("dep", ("sh", &["-c", "exit 0"])),
            )?,
        );

        let mut app_cfg = service_config("app", ("sh", &["-c", "stty size; read line; stty size"]));
        app_cfg.depends_on = vec![config::Dependency {
            name: spanned_string("dep"),
            condition: Some(Spanned {
                span: Default::default(),
                inner: config::DependencyCondition::ServiceCompletedSuccessfully,
            }),
        }];
        services.insert("app".to_string(), Service::new("app", config_dir, app_cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        commands_tx
            .send(Command::ResizeAll {
                cols: 100,
                rows: 40,
            })
            .await?;

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_first = false;
        let mut saw_second = false;
        for _ in 0..80 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::Started { service_id } if service_id == "app" => {}
                Event::LogLine {
                    service_id, line, ..
                } if service_id == "app" => {
                    if line.trim() == "40 100" {
                        if !saw_first {
                            saw_first = true;
                            commands_tx
                                .send(Command::SendInput("app".to_string(), b"go\r".to_vec()))
                                .await?;
                        } else {
                            saw_second = true;
                            break;
                        }
                    }
                }
                _ => {}
            }
        }

        assert!(saw_first);
        assert!(saw_second);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn working_dir_is_used_for_spawn() -> eyre::Result<()> {
        let base = unique_tmp_dir("working-dir");
        fs::create_dir_all(&base)?;
        let config_dir = base.join("cfg");
        let work_rel = "work";
        let work_abs = config_dir.join(work_rel);
        fs::create_dir_all(&work_abs)?;

        let mut cfg = service_config("svc", ("sh", &["-c", "pwd"]));
        cfg.working_dir = Some(spanned_string(work_rel));

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", &config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                    true,
                )
                .await
            }
        });

        let mut saw_pwd = false;
        let expected = work_abs.canonicalize()?;

        for _ in 0..50 {
            let ev = timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .map_err(|_| eyre::eyre!("timeout waiting for event"))?
                .ok_or_else(|| eyre::eyre!("event channel closed"))?;
            match ev {
                Event::LogLine { line, .. } => {
                    if Path::new(&line) == expected {
                        saw_pwd = true;
                        break;
                    }
                }
                Event::Exited(_, _) if saw_pwd => break,
                _ => {}
            }
        }

        assert!(
            saw_pwd,
            "did not observe expected pwd output {}",
            expected.display()
        );
        shutdown.cancel();
        handle.await??;
        Ok(())
    }
}
