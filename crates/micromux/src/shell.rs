//! Rendering an argv as one POSIX shell command line.

use std::ffi::OsStr;

/// Joins `argv` into a command line that a POSIX shell splits back into the same argv.
///
/// Every command micromux displays goes through this function, so a line copied from the TUI,
/// `ctl` or a log runs the command micromux ran when pasted into `sh`.
/// An argument stays bare when a shell takes every one of its characters literally.
/// Anything else, including an empty argument, whitespace, quotes, `$`, backslashes and other
/// metacharacters, is quoted.
///
/// The quoting is POSIX `sh` quoting on every platform, including Windows, where `cmd.exe` would
/// read the line differently.
/// Control characters such as the newlines of a multi-line `sh -c` script stay literal inside the
/// quotes, because POSIX `sh` has no portable escape for them, so a single-line view must render
/// them itself.
/// Non-UTF-8 arguments are converted lossily, so the line only approximates them.
///
/// # Examples
///
/// ```
/// let argv = ["sh", "-c", "echo 'ok'"];
/// assert_eq!(micromux::shell_join(argv), r#"sh -c "echo 'ok'""#);
/// ```
pub fn shell_join<S: AsRef<OsStr>>(argv: impl IntoIterator<Item = S>) -> String {
    argv.into_iter()
        .enumerate()
        .map(|(index, arg)| quote(&arg.as_ref().to_string_lossy(), index == 0))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quotes one argument; `command_word` marks the first word, where `name=value` would be read as
/// a variable assignment.
fn quote(arg: &str, command_word: bool) -> String {
    // A leading `=` is quoted too, because zsh, the default interactive shell on macOS, expands
    // `=name` to the path of the command `name`.
    let equals_literal = !command_word && !arg.starts_with('=');
    let literal = |ch: char| {
        ch.is_ascii_alphanumeric()
            || matches!(ch, '_' | '@' | '%' | '+' | ':' | ',' | '.' | '/' | '-')
            || (ch == '=' && equals_literal)
            // Non-ASCII letters and digits are literal; non-ASCII spaces and symbols are not
            // alphanumeric, so they are quoted.
            || (!ch.is_ascii() && ch.is_alphanumeric())
    };
    if !arg.is_empty() && arg.chars().all(literal) {
        return arg.to_string();
    }
    if !arg.contains('\'') {
        return format!("'{arg}'");
    }
    // Double quotes keep a single quote readable, but they expand `$` and backquotes, treat a
    // backslash as an escape, and an interactive bash expands `!`.
    if !arg.contains(['"', '$', '`', '\\', '!']) {
        return format!("\"{arg}\"");
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::shell_join;
    use similar_asserts::assert_eq;

    /// Splits `line` the way `sh` does, by running it and printing each argument NUL-terminated.
    #[cfg(unix)]
    fn sh_split(line: &str) -> Vec<String> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\0' {line}"))
            .output()
            .expect("run sh");
        assert!(output.status.success(), "sh rejected {line:?}: {output:?}");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 output");
        let mut words = stdout.split('\0').map(str::to_string).collect::<Vec<_>>();
        // `printf` terminates the last argument too, which leaves an empty tail.
        assert_eq!(words.pop().as_deref(), Some(""));
        words
    }

    /// Arguments covering every quoting case: shell metacharacters, both quote kinds, `$`,
    /// backslashes, globs, empty and whitespace-only arguments, control characters, and a
    /// multi-line script.
    const TRICKY: &[&str] = &[
        "sh",
        "-c",
        "echo 'ok'",
        r#"say "hi""#,
        "$HOME",
        r"C:\path\to",
        r"back\\slash",
        "it's a \"mix\" of $both `and` \\",
        "it's $HOME",
        "it's!",
        "*.rs",
        "a;b|c&d",
        "",
        " ",
        "\t",
        "line one\nline two",
        "bell\u{7}",
        "~user",
        "KEY=value",
        "--port=8000",
        "==",
        "+%Y",
        "!bang",
        "^caret",
        "#hash",
        "{a,b}",
        "naïve",
        "non\u{a0}breaking",
        "plain-arg_1.0/x:y@z+",
    ];

    #[test]
    fn shell_join_quotes_the_cmd_shell_script_as_one_argument() {
        // The `-c` script must read as a single word, not as `sh -c echo` plus extra arguments.
        assert_eq!(
            shell_join(["sh", "-c", "echo 'ok'"]),
            r#"sh -c "echo 'ok'""#
        );
    }

    #[test]
    fn shell_join_leaves_plain_arguments_bare_and_quotes_the_rest() {
        // Arguments a shell takes literally stay bare, so ordinary commands read naturally.
        assert_eq!(
            shell_join([
                "uvicorn",
                "app:main",
                "--port=8000",
                "+%Y",
                "a,b",
                "naïve.txt"
            ]),
            "uvicorn app:main --port=8000 +%Y a,b naïve.txt"
        );

        // Only the command word quotes `=`, where it would assign a variable, and so does a
        // leading `=`, which zsh expands to a command path.
        assert_eq!(
            shell_join(["KEY=value", "KEY=value"]),
            "'KEY=value' KEY=value"
        );
        assert_eq!(shell_join(["[", "a", "==", "b", "]"]), "'[' a '==' b ']'");

        // Empty and whitespace arguments survive as words of their own.
        assert_eq!(shell_join(["printf", "", " "]), "printf '' ' '");

        // `$`, backslashes and quotes are quoted so the shell neither expands nor strips them.
        assert_eq!(shell_join(["echo", "$HOME"]), "echo '$HOME'");
        assert_eq!(shell_join(["echo", r"a\b"]), r"echo 'a\b'");
        assert_eq!(shell_join(["echo", r#"say "hi""#]), r#"echo 'say "hi"'"#);
        assert_eq!(shell_join(["echo", "it's $HOME"]), r"echo 'it'\''s $HOME'");
    }

    #[test]
    fn shell_join_of_nothing_is_empty() {
        assert_eq!(shell_join(Vec::<String>::new()), "");
    }

    #[cfg(unix)]
    #[test]
    fn shell_join_round_trips_through_sh() {
        // A real POSIX shell splits the joined line back into exactly the original argv.
        let expected = TRICKY.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(sh_split(&shell_join(TRICKY)), expected);
    }

    #[test]
    fn shell_join_round_trips_through_shlex_split() {
        // The joined line also parses back the same way micromux parses a string `command`.
        let expected = TRICKY.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(shlex::split(&shell_join(TRICKY)), Some(expected));
    }
}
