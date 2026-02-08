use clap::Parser;
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
    #[clap(short = 'c', long = "config", help = "path to config file")]
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
}
