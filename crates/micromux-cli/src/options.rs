use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::LazyLock;

/// The release version followed by the control protocol version, which decides whether this
/// binary can talk to a running session or an MCP proxy built from another release.
static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        micromux_control::PROTOCOL_VERSION
    )
});

/// Logging flags to `#[command(flatten)]` into your CLI
#[derive(clap::Args, Debug, Clone, Copy, Default)]
pub struct Verbosity {
    #[arg(
        long,
        short = 'v',
        action = clap::ArgAction::Count,
        global = true,
        help = "Increase logging verbosity",
        long_help = None,
    )]
    pub verbose: u8,

    #[arg(
        long,
        short = 'q',
        action = clap::ArgAction::Count,
        global = true,
        help = "Decrease logging verbosity",
        long_help = None,
        conflicts_with = "verbose",
    )]
    pub quiet: u8,
}

#[derive(Debug, Parser)]
#[command(author, version = VERSION.as_str())]
pub struct Options {
    #[clap(
        short = 'c',
        long = "config",
        help = "path to config file",
        global = true
    )]
    pub config_path: Option<PathBuf>,

    #[arg(long = "strict", env = "MICROMUX_STRICT", help = "enable strict mode")]
    pub strict: Option<bool>,

    #[arg(
        long = "color",
        env = "MICROMUX_COLOR",
        help = "enable or disable color"
    )]
    pub color_choice: Option<termcolor::ColorChoice>,

    #[command(flatten)]
    pub verbosity: Verbosity,

    #[arg(
        long = "log",
        env = "MICROMUX_LOG_LEVEL",
        aliases = ["log-level"],
        help = "Log level. When using a more sophisticated logging setup using RUST_LOG environment variable, this option is overwritten."
    )]
    pub log_level: Option<tracing::metadata::Level>,

    #[arg(long = "log-file", env = "MICROMUX_LOG_FILE", help = "Log file")]
    pub log_file: Option<PathBuf>,

    #[arg(
        long = "no-pretty-json-logs",
        env = "MICROMUX_NO_PRETTY_JSON_LOGS",
        help = "show structured JSON logs as raw JSON in the TUI (also configurable via `ui: { pretty_json_logs: false }`)"
    )]
    pub no_pretty_json_logs: bool,

    #[arg(
        long = "mouse",
        env = "MICROMUX_MOUSE",
        global = true,
        value_name = "on|off",
        value_parser = clap::builder::BoolishValueParser::new(),
        help = "capture the mouse in the TUI for clicks, scrolling and selection; overrides `ui: { mouse: ... }` in the project and user configs"
    )]
    pub mouse: Option<bool>,

    #[arg(
        long = "copy-on-select",
        env = "MICROMUX_COPY_ON_SELECT",
        global = true,
        value_name = "on|off",
        value_parser = clap::builder::BoolishValueParser::new(),
        help = "copy a mouse selection in the TUI as soon as the drag ends instead of on y or Enter; overrides `ui: { copy_on_select: ... }` in the project and user configs"
    )]
    pub copy_on_select: Option<bool>,

    #[arg(
        long = "no-control",
        env = "MICROMUX_NO_CONTROL",
        help = "disable the agent control plane (also configurable via `control: { enabled: false }`)"
    )]
    pub no_control: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Options {
    /// The TUI settings the flags set, which take precedence over the configs.
    pub(crate) fn ui_setting_flags(&self) -> crate::ui_settings::Layers {
        crate::ui_settings::Layers {
            mouse: self.mouse,
            copy_on_select: self.copy_on_select,
        }
    }
}

/// A micromux subcommand. With no subcommand, micromux runs the TUI for the current project.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Attach the TUI to an already-running micromux session.
    Attach {
        /// Session selector: `name:`, `pid:`, `hash:`, or a bare session name.
        #[arg(long)]
        session: Option<String>,
    },
    /// Control a running micromux session over its local endpoint (dogfoods the control protocol).
    Ctl {
        /// The action to perform.
        #[command(subcommand)]
        action: CtlAction,
    },
    /// Run a command in this terminal with a running session's environment.
    ///
    /// The command keeps the variables of the shell that runs `micromux exec`, such as `PATH`,
    /// and the session's variables override any of the same name.
    /// It replaces the micromux process, so it gets the terminal, the signals, and the exit
    /// status directly.
    /// Exits 125 when micromux fails before the command runs, such as without a running session,
    /// 126 when the command cannot run, 127 when it is not found, and otherwise with the
    /// command's own status.
    Exec {
        /// Session selector: `name:`, `pid:`, `hash:`, or a bare session name.
        #[arg(long)]
        session: Option<String>,
        /// Use the environment of the service with this id instead of the session's variables.
        #[arg(long)]
        service: Option<String>,
        /// The command and its arguments; everything after the command name is passed to it.
        #[arg(
            value_name = "COMMAND",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<OsString>,
    },
    /// Print a running session's environment.
    ///
    /// Without `--service`, these are the variables the session injects into every service: its
    /// name, domain, proxy port, host suffix, and allocated ports.
    /// With `--service`, they are that service's resolved environment.
    /// Exits 1 on any failure, including a NAME that is not set.
    Env {
        /// Session selector: `name:`, `pid:`, `hash:`, or a bare session name.
        #[arg(long)]
        session: Option<String>,
        /// Print the environment of the service with this id instead of the session's variables.
        #[arg(long)]
        service: Option<String>,
        /// Print `export` lines for `eval "$(micromux env --export)"`, as `--format shell` does.
        #[arg(long, conflicts_with = "format")]
        export: bool,
        /// Output format [default: env].
        #[arg(long, value_enum)]
        format: Option<EnvFormat>,
        /// Print only these variables; a single NAME in the `env` format prints its bare value.
        #[arg(value_name = "NAME")]
        names: Vec<String>,
    },
    /// Run the MCP server over stdio (configure once in Claude Code / Codex like playwright-mcp).
    #[cfg(feature = "mcp")]
    Mcp {
        /// Permit detached session starts, rate-limited to eight requests per minute.
        #[arg(long, env = "MICROMUX_MCP_ALLOW_SESSION_START")]
        allow_session_start: bool,
    },
    /// Run the supervisor headless (no TUI), serving the control plane until stopped. Intended for
    /// agent-managed sessions — see the MCP `start_session`/`stop_session` tools.
    Serve,
    /// Run the shared HTTP proxy serving every session's `routes`. Sessions with routes start it
    /// automatically; it exits once no session has been registered for the idle timeout.
    Proxy {
        /// Port to listen on first; sessions have the proxy listen on their own ports as well.
        #[arg(long, default_value_t = 80)]
        port: u16,
        /// Port to use instead when `--port` needs privileges or is taken.
        #[arg(long)]
        fallback_port: Option<u16>,
        /// Seconds to keep running without any registered session.
        #[arg(long, default_value_t = micromux_proxy::protocol::IDLE_TIMEOUT.as_secs())]
        idle_timeout_secs: u64,
    },
}

/// How `micromux env` prints variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum EnvFormat {
    /// `KEY=value` lines, with values quoted for a POSIX shell where needed.
    Env,
    /// `export KEY=value` lines for `eval`.
    Shell,
    /// One JSON object.
    Json,
}

#[cfg(test)]
mod tests {
    use super::{Command, EnvFormat, Options};
    use clap::{CommandFactory as _, Parser};
    use similar_asserts::assert_eq;

    /// `--version` names the control protocol next to the release, so a mismatch between a
    /// session and a client can be read off both binaries.
    #[test]
    fn version_names_the_control_protocol() {
        let version = Options::command().render_version();
        let expected = format!(
            " {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            micromux_control::PROTOCOL_VERSION
        );
        assert!(version.trim_end().ends_with(&expected), "{version}");
    }

    fn exec_command(args: &[&str]) -> Result<(Option<String>, Vec<String>), clap::Error> {
        match Options::try_parse_from(args)?.command {
            Some(Command::Exec {
                service, command, ..
            }) => Ok((
                service,
                command
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned())
                    .collect(),
            )),
            other => panic!("expected exec, got {other:?}"),
        }
    }

    /// The command starts at its first word: flags before it belong to micromux, and every
    /// argument after it, flag or `--`, belongs to the command.
    #[test]
    fn exec_passes_everything_after_the_command_name_to_the_command() -> Result<(), clap::Error> {
        // Without `--`
        assert_eq!(
            exec_command(&["micromux", "exec", "task", "migrations:reset"])?,
            (
                None,
                vec!["task".to_string(), "migrations:reset".to_string()]
            )
        );
        assert_eq!(
            exec_command(&["micromux", "exec", "ls", "-la"])?,
            (None, vec!["ls".to_string(), "-la".to_string()])
        );

        // Flags after the command name never reach micromux.
        assert_eq!(
            exec_command(&["micromux", "exec", "sh", "-c", "x", "--service", "--", "-v"])?,
            (
                None,
                ["sh", "-c", "x", "--service", "--", "-v"]
                    .map(str::to_string)
                    .to_vec()
            )
        );

        // With micromux's own flags and `--`
        assert_eq!(
            exec_command(&["micromux", "exec", "--service", "api", "--", "ls", "-la"])?,
            (
                Some("api".to_string()),
                vec!["ls".to_string(), "-la".to_string()]
            )
        );
        let options = Options::try_parse_from([
            "micromux",
            "exec",
            "--config",
            "other.yaml",
            "--session",
            "name:api",
            "true",
        ])?;
        assert_eq!(
            options.config_path.as_deref(),
            Some(std::path::Path::new("other.yaml"))
        );
        assert!(matches!(
            options.command,
            Some(Command::Exec { session: Some(session), .. }) if session == "name:api"
        ));

        // A command is required.
        assert!(Options::try_parse_from(["micromux", "exec"]).is_err());
        assert!(Options::try_parse_from(["micromux", "exec", "--service", "api"]).is_err());
        Ok(())
    }

    #[test]
    fn env_takes_names_and_one_output_format() -> Result<(), clap::Error> {
        let options = Options::try_parse_from([
            "micromux",
            "env",
            "--service",
            "api",
            "--format",
            "json",
            "A",
            "B",
        ])?;
        assert!(matches!(
            options.command,
            Some(Command::Env {
                session: None,
                service: Some(service),
                export: false,
                format: Some(EnvFormat::Json),
                names,
            }) if service == "api" && names == ["A", "B"]
        ));

        let options = Options::try_parse_from(["micromux", "env", "--export"])?;
        assert!(matches!(
            options.command,
            Some(Command::Env {
                export: true,
                format: None,
                ..
            })
        ));

        // `--export` is `--format shell`, so it cannot be combined with another format.
        assert!(
            Options::try_parse_from(["micromux", "env", "--export", "--format", "json"]).is_err()
        );
        Ok(())
    }

    #[test]
    fn attach_accepts_optional_selector_and_global_config_override() -> Result<(), clap::Error> {
        let current = Options::try_parse_from(["micromux", "attach"]);
        assert!(matches!(
            current.map(|options| options.command),
            Ok(Some(Command::Attach { session: None }))
        ));

        let options = Options::try_parse_from([
            "micromux",
            "attach",
            "--session",
            "name:api",
            "--config",
            "other.yaml",
        ])?;
        assert_eq!(
            options.config_path.as_deref(),
            Some(std::path::Path::new("other.yaml"))
        );
        assert!(matches!(
            options.command,
            Some(Command::Attach { session: Some(session) }) if session == "name:api"
        ));
        Ok(())
    }
}

/// An action for the `micromux ctl` client.
#[derive(Debug, Subcommand)]
pub enum CtlAction {
    /// List the services in the session.
    Ls,
    /// Print recent log lines for a service.
    Logs {
        /// The service to read logs from.
        service: String,
        /// Read a specific retained run generation.
        #[arg(long)]
        run_generation: Option<u64>,
        /// Bound the result to the most recent lines.
        #[arg(long)]
        tail: Option<usize>,
    },
    /// List retained log runs for a service.
    LogRuns {
        /// The service to inspect.
        service: String,
    },
    /// Restart a service.
    Restart {
        /// The service to restart.
        service: String,
    },
    /// Restart all enabled services.
    RestartAll,
    /// Enable (and start) a service.
    Enable {
        /// The service to enable.
        service: String,
    },
    /// Disable a service.
    Disable {
        /// The service to disable.
        service: String,
    },
    /// Retire a dynamic service while preserving its post-mortem state.
    StopDynamic {
        /// The dynamic service to retire.
        service: String,
    },
    /// Reconcile configured services with the on-disk config.
    Reconcile {
        /// Show the semantic diff without changing the running session.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the latest healthcheck attempt for a service's current live run.
    Health {
        /// The service to inspect.
        service: String,
        /// Show the retained attempt history (oldest first) instead of only the latest attempt.
        #[arg(long)]
        history: bool,
    },
    /// Wait until a service is healthy, or until its run exits with `--exit`.
    ///
    /// Exits 0 once the condition holds (with `--exit`, once the run exits with status 0), 3 when
    /// the run exited instead (with `--exit`, with a non-zero status), 4 when the service is
    /// blocked behind a dependency that can no longer meet its condition, 5 when the service is
    /// disabled or retired, and 124 when the timeout elapses.
    Wait {
        /// The service to wait for.
        service: String,
        /// Wait until the service is up, and healthy if it has a healthcheck (the default).
        #[arg(long, conflicts_with = "exit")]
        healthy: bool,
        /// Wait until the service's run exits, as a one-shot job does.
        #[arg(long)]
        exit: bool,
        /// Give up after this long, such as `90s` or `5m`.
        #[arg(long, default_value = "60s", value_parser = humantime::parse_duration)]
        timeout: std::time::Duration,
        /// Accept only a run newer than this generation, such as the one `restart` printed.
        #[arg(long)]
        after_generation: Option<u64>,
    },
    /// Show the session identity.
    Describe,
    /// Stop the session: stop all services and exit, freeing its ports.
    Stop,
}
