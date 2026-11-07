//! `micromux env` and `micromux exec`: a running session's environment, printed or run with.
//!
//! Both read the environment from the live session over the control plane, so they show exactly
//! what its services got and can never disagree with each other.
//! Values may hold secrets from env files: they go to stdout or the command, and never into a log
//! or a diagnostic.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write as _;
use std::path::Path;

use micromux_control::{
    Client, ENVIRONMENT_PROTOCOL_VERSION, ErrorCode, PROTOCOL_VERSION, Request,
    ResolvedEnvironment, Response, SessionInfo, SessionSelector,
};

use crate::options::EnvFormat;

/// Exit code of `micromux env` on any failure.
const ENV_FAILED: i32 = 1;

/// Exit code of `micromux exec` when micromux fails before the command runs, as `env(1)` and
/// `timeout(1)` use.
const EXEC_FAILED: i32 = 125;

/// Which environment to read: the session's variables, or one service's.
pub(crate) struct Target<'a> {
    /// Session selector; `None` selects the session for the config resolved from the current
    /// directory.
    pub(crate) session: Option<&'a str>,
    /// Service id; `None` selects the session's own variables.
    pub(crate) service: Option<&'a str>,
    /// Explicit config file, used when no selector names the session.
    pub(crate) config: Option<&'a Path>,
}

/// What `micromux env` prints.
pub(crate) struct Output<'a> {
    pub(crate) format: EnvFormat,
    pub(crate) names: &'a [String],
}

/// Prints the target's environment to stdout.
///
/// # Errors
///
/// Returns [`crate::Error::Exit`] with code 1 once the failure is reported on stderr.
pub(crate) async fn print(target: &Target<'_>, output: &Output<'_>) -> Result<(), crate::Error> {
    let result = async {
        let (session, environment) = fetch(target, "env").await?;
        let selected = select(&environment.variables, output.names)
            .map_err(|missing| missing_message(&missing, &session, target.service))?;
        let bare = output.format == EnvFormat::Env && output.names.len() == 1;
        let rendered = render(&selected, output.format, bare)?;
        // A name the caller asked for must not silently go missing, as a missing one does not.
        if !output.names.is_empty() && !rendered.skipped.is_empty() {
            return Err(crate::Error::Message(format!(
                "cannot print {} as shell assignments: not valid shell variable names; use \
                 --format json",
                rendered
                    .skipped
                    .iter()
                    .map(|key| format!("{key:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        for key in &rendered.skipped {
            eprintln!("micromux: skipping {key:?}, which is not a valid shell variable name");
        }
        write_stdout(&rendered.text)
    }
    .await;
    result.map_err(|err| report(err, ENV_FAILED))
}

/// Replaces this process with `command`, run with the target's environment over the caller's.
///
/// # Errors
///
/// Returns [`crate::Error::Exit`] once the failure is reported on stderr: code 125 when the
/// environment cannot be read, 126 when the command cannot run, and 127 when it is not found.
/// On success it does not return.
pub(crate) async fn exec(target: &Target<'_>, command: &[OsString]) -> Result<(), crate::Error> {
    let (_, environment) = fetch(target, "exec")
        .await
        .map_err(|err| report(err, EXEC_FAILED))?;
    Err(replace_process(command, environment.variables))
}

/// Prints `err` unless it was already reported, and exits with `code`.
fn report(err: crate::Error, code: i32) -> crate::Error {
    match err {
        crate::Error::Exit(code) => crate::Error::Exit(code),
        other => {
            eprintln!("micromux: {other}");
            crate::Error::Exit(code)
        }
    }
}

/// Reads the target's environment from its live session.
async fn fetch(
    target: &Target<'_>,
    command: &str,
) -> Result<(SessionInfo, ResolvedEnvironment), crate::Error> {
    let (mut client, session) = connect(target, command).await?;
    if !session
        .protocol_version
        .supports(ENVIRONMENT_PROTOCOL_VERSION)
    {
        return Err(crate::Error::Message(format!(
            "the running session `{}` (protocol {}) is too old for `micromux {command}`; restart \
             it with this micromux (protocol {PROTOCOL_VERSION})",
            session.name, session.protocol_version
        )));
    }
    let response = client
        .request(Request::GetEnvironment {
            service: target.service.map(str::to_string),
        })
        .await?;
    let environment = match response {
        Response::Environment(environment) => environment,
        Response::Error {
            code: ErrorCode::UnknownService,
            ..
        } => {
            return Err(crate::Error::Message(format!(
                "session `{}` has no service with id `{}`; `--service` takes an id as `micromux \
                 ctl ls` lists it",
                session.name,
                target.service.unwrap_or_default()
            )));
        }
        Response::Error { code, message } => {
            return Err(crate::Error::Message(format!("{code:?}: {message}")));
        }
        // The environment response is the only one that carries values, so printing any other
        // response cannot disclose one.
        other => {
            return Err(crate::Error::Message(format!(
                "unexpected response to an environment query: {other:?}"
            )));
        }
    };
    if environment.stale
        && let Some(service) = target.service
    {
        eprintln!(
            "micromux: {service}'s config changed since it started; showing the environment of \
             its running process"
        );
    }
    Ok((session, environment))
}

/// Connects to the selected session, or, without a selector, to the one for the project's
/// config exactly as `micromux ctl` does.
async fn connect(
    target: &Target<'_>,
    command: &str,
) -> Result<(Client, SessionInfo), crate::Error> {
    let selector = target
        .session
        .map(SessionSelector::parse)
        .filter(|selector| *selector != SessionSelector::Current);
    let Some(selector) = selector else {
        let session = crate::ctl::connect_project(target.config).await?;
        return Ok((session.client, session.info));
    };
    let cwd = std::env::current_dir()?;
    let resolved = micromux_control::resolve_selector(&cwd, Some(selector), target.config)
        .await
        .map_err(|err| crate::Error::Message(crate::attach::format_select_error(err, command)))?;
    let client = Client::connect(&resolved.endpoint)
        .await
        .map_err(|err| crate::Error::Message(crate::attach::format_control_error(err, command)))?;
    Ok((client, resolved.info))
}

fn missing_message(missing: &[&str], session: &SessionInfo, service: Option<&str>) -> crate::Error {
    let scope = service.map_or_else(
        || format!("session `{}`", session.name),
        |service| {
            format!(
                "the environment of `{service}` in session `{}`",
                session.name
            )
        },
    );
    crate::Error::Message(format!("not set in {scope}: {}", missing.join(", ")))
}

fn write_stdout(text: &str) -> Result<(), crate::Error> {
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        // A reader such as `head` that stops early is not a failure of ours.
        Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        result => Ok(result?),
    }
}

/// The requested variables, or the requested names that are not set.
///
/// Without names, every variable is selected.
fn select<'a>(
    variables: &'a BTreeMap<String, String>,
    names: &'a [String],
) -> Result<BTreeMap<&'a str, &'a str>, Vec<&'a str>> {
    let missing = names
        .iter()
        .filter(|name| !variables.contains_key(name.as_str()))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(missing);
    }
    Ok(variables
        .iter()
        .filter(|(key, _)| names.is_empty() || names.contains(key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect())
}

/// The text `micromux env` prints, and the keys it left out.
#[derive(Debug, PartialEq, Eq)]
struct Rendered {
    text: String,
    /// Keys a shell cannot assign, left out of the `env` and `shell` formats.
    skipped: Vec<String>,
}

/// Renders the selected variables sorted by name; `bare` prints only the single value, unquoted,
/// so `$(micromux env NAME)` works.
fn render(
    selected: &BTreeMap<&str, &str>,
    format: EnvFormat,
    bare: bool,
) -> Result<Rendered, serde_json::Error> {
    let mut rendered = Rendered {
        text: String::new(),
        skipped: Vec::new(),
    };
    if bare {
        for value in selected.values() {
            rendered.text.push_str(value);
            rendered.text.push('\n');
        }
        return Ok(rendered);
    }
    let prefix = match format {
        EnvFormat::Env => "",
        EnvFormat::Shell => "export ",
        EnvFormat::Json => {
            rendered.text = serde_json::to_string_pretty(selected)?;
            rendered.text.push('\n');
            return Ok(rendered);
        }
    };
    for (key, value) in selected {
        if is_shell_name(key) {
            rendered.text.push_str(prefix);
            rendered.text.push_str(key);
            rendered.text.push('=');
            rendered.text.push_str(&shell_quote(value));
            rendered.text.push('\n');
        } else {
            rendered.skipped.push((*key).to_string());
        }
    }
    Ok(rendered)
}

/// Whether a POSIX shell can assign `key`: `[A-Za-z_][A-Za-z0-9_]*`.
fn is_shell_name(key: &str) -> bool {
    let mut bytes = key.bytes();
    bytes
        .next()
        .is_some_and(|first| first == b'_' || first.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
}

/// Quotes `value` for a POSIX shell, leaving values made only of unambiguous characters bare.
///
/// Inside single quotes nothing is special, so an embedded single quote ends the quoting, adds an
/// escaped quote, and resumes it.
fn shell_quote(value: &str) -> Cow<'_, str> {
    let bare = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte));
    if bare {
        Cow::Borrowed(value)
    } else {
        Cow::Owned(format!("'{}'", value.replace('\'', r"'\''")))
    }
}

/// Execs `command` with `variables` layered over the inherited environment.
///
/// Every descriptor micromux opened is close-on-exec, and the runtime's threads end with the
/// exec, so the command inherits only the caller's stdio and process group.
#[cfg(unix)]
fn replace_process(command: &[OsString], variables: BTreeMap<String, String>) -> crate::Error {
    use std::os::unix::process::CommandExt as _;

    /// Exit code when the command exists but cannot run, as a shell reports it.
    const CANNOT_RUN: i32 = 126;
    /// Exit code when the command is not found, as a shell reports it.
    const NOT_FOUND: i32 = 127;

    let Some((program, args)) = command.split_first() else {
        eprintln!("micromux: no command to run");
        return crate::Error::Exit(EXEC_FAILED);
    };
    let err = std::process::Command::new(program)
        .args(args)
        .envs(variables)
        .exec();
    eprintln!(
        "micromux: cannot run `{}`: {err}",
        program.to_string_lossy()
    );
    crate::Error::Exit(if err.kind() == std::io::ErrorKind::NotFound {
        NOT_FOUND
    } else {
        CANNOT_RUN
    })
}

#[cfg(not(unix))]
fn replace_process(_command: &[OsString], _variables: BTreeMap<String, String>) -> crate::Error {
    eprintln!("micromux: `micromux exec` is not supported on this platform");
    crate::Error::Exit(EXEC_FAILED)
}

#[cfg(test)]
mod tests {
    use super::{EnvFormat, Rendered, is_shell_name, render, select, shell_quote};
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::collections::BTreeMap;

    fn variables(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    fn rendered(text: &str, skipped: &[&str]) -> Rendered {
        Rendered {
            text: text.to_string(),
            skipped: skipped.iter().map(ToString::to_string).collect(),
        }
    }

    /// Selects `names` and renders them the way `micromux env` does.
    fn env(
        variables: &BTreeMap<String, String>,
        names: &[String],
        format: EnvFormat,
    ) -> eyre::Result<Rendered> {
        let selected =
            select(variables, names).map_err(|missing| eyre::eyre!("missing {missing:?}"))?;
        let bare = format == EnvFormat::Env && names.len() == 1;
        Ok(render(&selected, format, bare)?)
    }

    #[test]
    fn shell_quote_leaves_plain_values_bare_and_quotes_the_rest() {
        // Bare
        assert_eq!(shell_quote("20001"), "20001");
        assert_eq!(
            shell_quote("postgres://user@127.0.0.1:5432/db"),
            "postgres://user@127.0.0.1:5432/db"
        );
        assert_eq!(shell_quote("a=b,c+d%e_f-g"), "a=b,c+d%e_f-g");

        // Quoted
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("line\nbreak"), "'line\nbreak'");
        assert_eq!(shell_quote("$HOME"), "'$HOME'");
        assert_eq!(shell_quote("~/dir"), "'~/dir'");
        assert_eq!(shell_quote("a\"b"), "'a\"b'");
    }

    /// Every quoted value reads back unchanged through a real shell.
    #[cfg(unix)]
    #[test]
    fn shell_quote_round_trips_through_sh() -> eyre::Result<()> {
        for value in [
            "",
            "plain",
            "two words",
            "it's",
            "a'b'c",
            "line\nbreak",
            "$(echo x)",
            "\\",
        ] {
            let script = format!("printf %s {}", shell_quote(value));
            let output = std::process::Command::new("sh")
                .args(["-c", &script])
                .output()?;
            assert_eq!(String::from_utf8_lossy(&output.stdout), value);
        }
        Ok(())
    }

    #[test]
    fn shell_names_follow_the_posix_identifier_rule() {
        for valid in ["A", "_", "_A1", "api_port", "X9"] {
            assert!(is_shell_name(valid), "{valid}");
        }
        for invalid in ["", "1A", "A-B", "A.B", "A B", "Ä"] {
            assert!(!is_shell_name(invalid), "{invalid}");
        }
    }

    #[test]
    fn env_prints_sorted_lines_and_skips_names_a_shell_cannot_assign() -> eyre::Result<()> {
        let variables = variables(&[("B", "two words"), ("A", "1"), ("NOT-SHELL", "x")]);

        // Default lines
        assert_eq!(
            env(&variables, &[], EnvFormat::Env)?,
            rendered("A=1\nB='two words'\n", &["NOT-SHELL"])
        );

        // Export lines
        assert_eq!(
            env(&variables, &[], EnvFormat::Shell)?,
            rendered("export A=1\nexport B='two words'\n", &["NOT-SHELL"])
        );
        Ok(())
    }

    #[test]
    fn env_json_carries_every_key() -> eyre::Result<()> {
        let variables = variables(&[("B", "it's"), ("NOT-SHELL", "x")]);

        let rendered = env(&variables, &[], EnvFormat::Json)?;

        assert_eq!(rendered.skipped, Vec::<String>::new());
        assert_eq!(
            serde_json::from_str::<BTreeMap<String, String>>(&rendered.text)?,
            variables
        );
        Ok(())
    }

    #[test]
    fn env_filters_by_name_and_prints_one_bare_value() -> eyre::Result<()> {
        let variables = variables(&[("A", "one value"), ("B", "2"), ("C", "3")]);

        // One name in the default format is its bare, unquoted value.
        assert_eq!(
            env(&variables, &names(&["A"]), EnvFormat::Env)?,
            rendered("one value\n", &[])
        );

        // Several names, or another format, keep the format's shape.
        assert_eq!(
            env(&variables, &names(&["C", "A"]), EnvFormat::Env)?,
            rendered("A='one value'\nC=3\n", &[])
        );
        assert_eq!(
            env(&variables, &names(&["B"]), EnvFormat::Shell)?,
            rendered("export B=2\n", &[])
        );
        assert_eq!(
            env(&variables, &names(&["B"]), EnvFormat::Json)?,
            rendered("{\n  \"B\": \"2\"\n}\n", &[])
        );
        Ok(())
    }

    /// A missing name fails the whole selection, so a script never reads an empty port.
    #[test]
    fn select_reports_every_missing_name() {
        let variables = variables(&[("A", "1")]);

        assert_eq!(
            select(&variables, &names(&["MISSING", "A", "ALSO"])),
            Err(vec!["MISSING", "ALSO"])
        );
    }
}
