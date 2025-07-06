use color_eyre::eyre;
use std::path::Path;
use termcolor::ColorChoice;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt::writer::BoxMakeWriter, layer::SubscriberExt};

#[derive(Debug)]
pub enum LogFile<'a> {
    RollingLog { cache_dir: &'a Path },
    LogFile { path: &'a Path },
}

/// Setup logging
///
/// # Errors
/// - If the logging directive cannot be parsed.
/// - If the global tracing subscriber cannot be installed.
pub fn setup<'a>(
    log_level: Option<tracing::metadata::Level>,
    log_file: LogFile<'a>,
) -> eyre::Result<tracing_appender::non_blocking::WorkerGuard> {
    let (log_writer, guard) = match log_file {
        LogFile::RollingLog { cache_dir } => {
            let file_appender =
                RollingFileAppender::new(Rotation::DAILY, cache_dir, "micromux.log");
            tracing_appender::non_blocking(file_appender)
        }
        LogFile::LogFile { path } => {
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(path)?;
            dbg!(&path);
            tracing_appender::non_blocking(log_file)
        }
    };

    let default_log_level = log_level.unwrap_or(tracing::metadata::Level::WARN);
    let default_log_directive = ["none".to_string()]
        .into_iter()
        .chain(
            ["micromux_cli", "micromux_tui", "micromux"]
                .into_iter()
                .map(|pkg| {
                    format!(
                        "{pkg}={}",
                        default_log_level.to_string().to_ascii_lowercase()
                    )
                }),
        )
        .collect::<Vec<String>>()
        .join(",");

    dbg!(&default_log_directive);
    let default_env_filter = tracing_subscriber::filter::EnvFilter::builder()
        .with_regex(true)
        .with_default_directive(default_log_level.into())
        .parse(default_log_directive)?;

    let use_rust_log_override = std::env::var(EnvFilter::DEFAULT_ENV)
        .ok()
        .map(|s| !s.is_empty())
        .is_some();
    let env_filter = if use_rust_log_override {
        match tracing_subscriber::filter::EnvFilter::builder()
            .with_env_var(EnvFilter::DEFAULT_ENV)
            .try_from_env()
        {
            Ok(env_filter) => env_filter,
            Err(err) => {
                eprintln!("invalid log filter: {err}");
                eprintln!("falling back to default logging");
                default_env_filter
            }
        }
    } else {
        default_env_filter
    };

    let fmt_layer_pretty_compact = tracing_subscriber::fmt::Layer::new()
        .compact()
        // .without_time()
        .with_ansi(false)
        .with_writer(log_writer);

    let subscriber = tracing_subscriber::registry()
        .with(fmt_layer_pretty_compact)
        .with(env_filter);
    tracing::subscriber::set_global_default(subscriber)?;
    Ok(guard)
}
