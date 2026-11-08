//! Rendering ANSI text to a PNG with [`freeze`](https://github.com/charmbracelet/freeze).

use crate::Error;
use crate::theme::Theme;
use std::path::Path;

/// Fail early with a helpful message if `freeze` is not installed.
pub(crate) fn ensure_freeze() -> Result<(), Error> {
    let ok = std::process::Command::new("freeze")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(Error::FreezeMissing)
    }
}

/// Render a captured ANSI file to a PNG with `freeze`, using window chrome that matches the docs.
pub(crate) fn run_freeze(input: &Path, output: &Path, theme: &Theme) -> Result<(), Error> {
    let (r, g, b) = theme.background;
    let status = std::process::Command::new("freeze")
        .arg("--language")
        .arg("text")
        .arg("--output")
        .arg(output)
        .arg("--background")
        .arg(format!("#{r:02x}{g:02x}{b:02x}"))
        .arg("--window")
        .arg("--shadow.blur")
        .arg("20")
        .arg("--border.radius")
        .arg("8")
        .arg("--padding")
        .arg("20")
        .arg(input)
        // freeze reads its input from stdin when stdin is not a terminal, and would wait forever.
        .stdin(std::process::Stdio::null())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(Error::FreezeFailed {
            input: input.to_path_buf(),
        })
    }
}
