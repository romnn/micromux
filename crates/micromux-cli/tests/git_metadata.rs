//! User/project metadata policy exercised through the real headless binary.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use color_eyre::eyre;
use indoc::indoc;
use similar_asserts::assert_eq;

mod common;

struct Running {
    child: Child,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Err(err) = self.child.kill() {
            eprintln!("failed to stop the test supervisor: {err}");
        }
        if let Err(err) = self.child.wait() {
            eprintln!("failed to reap the test supervisor: {err}");
        }
    }
}

fn fake_commands(directory: &Path) -> eyre::Result<()> {
    let bin = directory.join("bin");
    std::fs::create_dir_all(&bin)?;
    let git = indoc! {r#"
        #!/bin/sh
        [ "$*" = 'symbolic-ref --quiet --short HEAD' ] || exit 1
        printf 'git\n' >> "$MICROMUX_TEST_COMMAND_LOG"
        printf 'feature/metadata\n'
    "#};
    let gh = indoc! {r#"
        #!/bin/sh
        [ "$*" = 'pr list --head feature/metadata --state open --json number --limit 1' ] || exit 1
        printf 'gh\n' >> "$MICROMUX_TEST_COMMAND_LOG"
        printf '[{"number":42}]\n'
    "#};
    for (name, script) in [("git", git), ("gh", gh)] {
        let path = bin.join(name);
        std::fs::write(&path, script)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn setting(enabled: Option<bool>) -> String {
    enabled
        .map(|enabled| format!("git:\n  metadata: {enabled}\n"))
        .unwrap_or_default()
}

async fn connect(
    directory: &Path,
    process: &mut Running,
) -> eyre::Result<micromux_control::Client> {
    let config = micromux_control::CanonicalConfigPath::new(directory.join("micromux.yaml"))?;
    let endpoint = micromux_control::endpoint_for(
        &micromux_control::runtime_dir_in(&common::state_directory(directory)),
        &config,
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(client) = micromux_control::Client::connect(&endpoint).await {
            return Ok(client);
        }
        eyre::ensure!(
            process.child.try_wait()?.is_none(),
            "supervisor exited before becoming ready"
        );
        eyre::ensure!(
            tokio::time::Instant::now() < deadline,
            "supervisor did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn project_metadata_policy_overrides_the_user_default() -> eyre::Result<()> {
    // Commands are recording stubs, so this covers the CLI boundary without GitHub access.
    for user in [None, Some(false), Some(true)] {
        for project in [None, Some(false), Some(true)] {
            let directory = tempfile::Builder::new()
                .prefix("mm-git-")
                .tempdir_in("/tmp")?;
            let path = directory.path();
            fake_commands(path)?;
            std::fs::create_dir_all(path.join("user/micromux"))?;
            std::fs::write(path.join("user/micromux/config.yaml"), setting(user))?;
            std::fs::write(
                path.join("micromux.yaml"),
                format!("version: 1\n{}services: {{}}\n", setting(project)),
            )?;
            let child = common::micromux(path)
                .arg("serve")
                .current_dir(path)
                .env("XDG_CONFIG_HOME", path.join("user"))
                .env("PATH", path.join("bin"))
                .env("MICROMUX_TEST_COMMAND_LOG", path.join("commands.log"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let mut process = Running { child };
            let mut client = connect(path, &mut process).await?;
            let enabled = project.or(user).unwrap_or(true);
            let mut git = client
                .describe()
                .await?
                .session
                .and_then(|session| session.git);
            if enabled {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while git.as_ref().and_then(|git| git.pull_request).is_none() {
                    eyre::ensure!(
                        tokio::time::Instant::now() < deadline,
                        "PR metadata did not arrive: user={user:?}, project={project:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    git = client
                        .describe()
                        .await?
                        .session
                        .and_then(|session| session.git);
                }
                let git = git.ok_or_else(|| eyre::eyre!("missing git context"))?;
                assert_eq!(git.branch, "feature/metadata");
                assert_eq!(git.pull_request, Some(42));
                let commands = std::fs::read_to_string(path.join("commands.log"))?;
                assert!(commands.contains("git\n"));
                assert_eq!(commands.lines().filter(|line| *line == "gh").count(), 1);
            } else {
                assert_eq!(git, None, "user={user:?}, project={project:?}");
                assert!(
                    !path.join("commands.log").exists(),
                    "disabled metadata launched a command"
                );
            }
        }
    }
    Ok(())
}
