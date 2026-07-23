use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
#[command(author, version)]
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
        long = "no-control",
        env = "MICROMUX_NO_CONTROL",
        help = "disable the agent control plane (also configurable via `control: { enabled: false }`)"
    )]
    pub no_control: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
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
    /// Run the MCP server over stdio (configure once in Claude Code / Codex like playwright-mcp).
    #[cfg(feature = "mcp")]
    Mcp,
    /// Run the supervisor headless (no TUI), serving the control plane until stopped. Intended for
    /// agent-managed sessions — see the MCP `start_session`/`stop_session` tools.
    Serve,
}

#[cfg(test)]
mod tests {
    use super::{Command, Options};
    use clap::Parser;
    use similar_asserts::assert_eq;

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
    },
    /// Show the session identity.
    Describe,
    /// Stop the session: stop all services and exit, freeing its ports.
    Stop,
}
