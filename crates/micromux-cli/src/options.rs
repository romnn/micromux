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
    /// Show the latest healthcheck attempt for a service.
    Health {
        /// The service to inspect.
        service: String,
    },
    /// Show the session identity.
    Describe,
    /// Stop the session: stop all services and exit, freeing its ports.
    Stop,
}
