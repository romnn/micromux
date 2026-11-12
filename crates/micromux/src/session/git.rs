//! Best-effort Git context with bounded, coalesced GitHub lookups.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tracing::Instrument as _;

const FOUND_TTL: Duration = Duration::from_hours(1);
const RETRY_TTL: Duration = Duration::from_mins(5);
const GIT_TIMEOUT: Duration = Duration::from_millis(250);
const GH_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENTRIES: usize = 256;

static CACHE: LazyLock<Arc<GitCache<Commands>>> =
    LazyLock::new(|| Arc::new(GitCache::new(Commands)));

/// Optional context for the checked-out branch of a session's working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitContext {
    /// Current local branch; detached HEAD and non-repositories have no context.
    pub branch: String,
    /// Number of an open GitHub pull request, when a background lookup has found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u64>,
}

impl GitContext {
    /// Reads the branch and schedules a cached, best-effort pull request lookup.
    ///
    /// Git gets up to 250 ms on each call; GitHub requests never delay the response.
    /// Each process shares a cache keyed by directory and branch, with at most four concurrent
    /// GitHub lookups, each limited to five seconds.
    /// Found PRs expire after an hour; missing PRs and failures retry after five minutes.
    /// A cold lookup returns the branch alone until a later call observes its result.
    pub async fn discover(directory: &Path) -> Option<Self> {
        CACHE.context(directory).await
    }
}

impl crate::Micromux {
    /// Sets the resolved project/user policy before starting the session.
    ///
    /// Disabled sessions run neither Git nor GitHub lookups and expose no Git metadata.
    #[must_use]
    pub fn with_git_metadata(mut self, enabled: bool) -> Self {
        self.git_metadata_enabled = enabled;
        self
    }

    /// Whether adapters may fetch this session's Git metadata.
    #[must_use]
    pub fn git_metadata_enabled(&self) -> bool {
        self.git_metadata_enabled
    }
}

impl super::SessionStatus {
    pub(crate) fn with_git_metadata(mut self, enabled: bool) -> Self {
        self.git_metadata_enabled = enabled;
        self
    }

    /// Reads Git context only when the session's resolved configuration permits fetching it.
    ///
    /// Disabled sessions return `None` without reading Git or the cached PR number.
    pub async fn git_context(&self, directory: &Path) -> Option<GitContext> {
        if !self.git_metadata_enabled {
            return None;
        }
        GitContext::discover(directory).await
    }
}

trait GitProvider: Send + Sync + 'static {
    fn branch(&self, directory: &Path) -> impl Future<Output = Option<String>> + Send;
    fn pull_request(&self, key: &CacheKey) -> impl Future<Output = Option<u64>> + Send;
}

struct Commands;

impl GitProvider for Commands {
    async fn branch(&self, directory: &Path) -> Option<String> {
        let output = command_output(
            Command::new("git")
                .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
                .current_dir(directory),
            GIT_TIMEOUT,
        )
        .await?;
        let branch = String::from_utf8(output).ok()?.trim().to_string();
        (!branch.is_empty()).then_some(branch)
    }

    async fn pull_request(&self, key: &CacheKey) -> Option<u64> {
        #[derive(Deserialize)]
        struct PullRequest {
            number: u64,
        }
        let output = command_output(
            Command::new("gh")
                .args(["pr", "list", "--head", &key.branch, "--state", "open"])
                .args(["--json", "number", "--limit", "1"])
                .env("GH_PROMPT_DISABLED", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .current_dir(&key.directory),
            GH_TIMEOUT,
        )
        .await?;
        let requests: Vec<PullRequest> = serde_json::from_slice(&output).ok()?;
        requests
            .first()
            .map(|request| request.number)
            .filter(|number| *number > 0)
    }
}

async fn command_output(command: &mut Command, timeout: Duration) -> Option<Vec<u8>> {
    let output = tokio::time::timeout(
        timeout,
        command
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    output.status.success().then_some(output.stdout)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    directory: PathBuf,
    branch: String,
}

struct CachedPullRequest {
    number: Option<u64>,
    refresh_at: Instant,
}

struct GitCache<P> {
    provider: P,
    entries: Mutex<HashMap<CacheKey, CachedPullRequest>>,
    lookups: Arc<Semaphore>,
}

impl<P: GitProvider> GitCache<P> {
    fn new(provider: P) -> Self {
        Self {
            provider,
            entries: Mutex::new(HashMap::new()),
            lookups: Arc::new(Semaphore::new(4)),
        }
    }

    async fn context(self: &Arc<Self>, directory: &Path) -> Option<GitContext> {
        let branch = self.provider.branch(directory).await?;
        let key = CacheKey {
            directory: directory.to_path_buf(),
            branch: branch.clone(),
        };
        let pull_request = self.cached_pull_request(key);
        Some(GitContext {
            branch,
            pull_request,
        })
    }

    fn cached_pull_request(self: &Arc<Self>, key: CacheKey) -> Option<u64> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| now < entry.refresh_at + FOUND_TTL);
        if let Some(entry) = entries.get(&key)
            && now < entry.refresh_at
        {
            return entry.number;
        }
        let number = entries.get(&key).and_then(|entry| entry.number);
        if !entries.contains_key(&key) && entries.len() >= MAX_ENTRIES {
            return None;
        }
        let Ok(permit) = Arc::clone(&self.lookups).try_acquire_owned() else {
            return number;
        };
        // Reserve the retry window before spawning, so concurrent callers share one lookup.
        // It also bounds retries if the runtime cancels a refresh before it can publish.
        entries.insert(
            key.clone(),
            CachedPullRequest {
                number,
                refresh_at: now + RETRY_TTL,
            },
        );
        drop(entries);
        let cache = Arc::clone(self);
        tokio::spawn(
            async move {
                let number = cache.provider.pull_request(&key).await;
                let ttl = if number.is_some() {
                    FOUND_TTL
                } else {
                    RETRY_TTL
                };
                cache.entries.lock().insert(
                    key,
                    CachedPullRequest {
                        number,
                        refresh_at: Instant::now() + ttl,
                    },
                );
                drop(permit);
            }
            .instrument(tracing::Span::current()),
        );
        number
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeGit {
        branch: Mutex<Option<String>>,
        number: Mutex<Option<u64>>,
        calls: AtomicUsize,
        branch_reads: AtomicUsize,
        gate: Semaphore,
        started: Semaphore,
    }

    impl FakeGit {
        fn new(number: Option<u64>) -> Self {
            Self {
                branch: Mutex::new(Some("feature/one".to_string())),
                number: Mutex::new(number),
                calls: AtomicUsize::new(0),
                branch_reads: AtomicUsize::new(0),
                gate: Semaphore::new(0),
                started: Semaphore::new(0),
            }
        }
    }

    impl GitProvider for FakeGit {
        fn branch(&self, _directory: &Path) -> impl Future<Output = Option<String>> + Send {
            self.branch_reads.fetch_add(1, Ordering::SeqCst);
            std::future::ready(self.branch.lock().clone())
        }

        async fn pull_request(&self, _key: &CacheKey) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            self.gate.acquire().await.ok()?.forget();
            *self.number.lock()
        }
    }

    async fn finish_refresh(cache: &GitCache<FakeGit>) -> eyre::Result<()> {
        cache.provider.gate.add_permits(4);
        // Every refresh holds a permit until it has published its result.
        let permits = cache.lookups.acquire_many(4).await?;
        drop(permits);
        Ok(())
    }

    async fn context(cache: &Arc<GitCache<FakeGit>>) -> eyre::Result<GitContext> {
        cache
            .context(Path::new("/project"))
            .await
            .ok_or_else(|| eyre::eyre!("missing branch context"))
    }

    #[tokio::test(start_paused = true)]
    async fn successful_results_refresh_after_an_hour() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(Some(42))));
        assert_eq!(context(&cache).await?.pull_request, None);
        finish_refresh(&cache).await?;
        assert_eq!(context(&cache).await?.pull_request, Some(42));

        *cache.provider.number.lock() = Some(43);
        tokio::time::advance(FOUND_TTL.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 1);

        // Reusing the same branch for another PR is picked up without restarting.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        finish_refresh(&cache).await?;
        assert_eq!(context(&cache).await?.pull_request, Some(43));
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 2);
        assert_eq!(cache.provider.branch_reads.load(Ordering::SeqCst), 5);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn missing_prs_and_failures_retry_after_five_minutes() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(None)));
        context(&cache).await?;
        finish_refresh(&cache).await?;
        *cache.provider.number.lock() = Some(42);
        tokio::time::advance(RETRY_TTL.saturating_sub(Duration::from_secs(1))).await;
        assert_eq!(context(&cache).await?.pull_request, None);
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(Duration::from_secs(1)).await;
        context(&cache).await?;
        finish_refresh(&cache).await?;
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn branch_changes_never_reuse_another_branches_pr() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(Some(42))));
        context(&cache).await?;
        finish_refresh(&cache).await?;

        *cache.provider.branch.lock() = Some("feature/two".to_string());
        *cache.provider.number.lock() = Some(43);
        let changed = context(&cache).await?;
        assert_eq!(changed.branch, "feature/two");
        assert_eq!(changed.pull_request, None);
        finish_refresh(&cache).await?;
        assert_eq!(context(&cache).await?.pull_request, Some(43));

        *cache.provider.branch.lock() = Some("feature/one".to_string());
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[tokio::test]
    async fn simultaneous_requests_share_a_nonblocking_refresh() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(Some(42))));
        let results = futures::future::join_all((0..20).map(|_| context(&cache))).await;
        for result in results {
            assert_eq!(result?.pull_request, None);
        }
        // The remote lookup remains blocked while all callers already have their branch.
        cache.provider.started.acquire().await?.forget();
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 1);
        finish_refresh(&cache).await?;
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        Ok(())
    }

    #[tokio::test]
    async fn directories_with_the_same_branch_are_isolated() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(Some(42))));
        context(&cache).await?;
        finish_refresh(&cache).await?;
        *cache.provider.number.lock() = Some(99);
        let other = Path::new("/other-project");
        assert_eq!(
            cache.context(other).await.map(|git| git.pull_request),
            Some(None)
        );
        finish_refresh(&cache).await?;
        assert_eq!(
            cache.context(other).await.map(|git| git.pull_request),
            Some(Some(99))
        );
        assert_eq!(context(&cache).await?.pull_request, Some(42));
        Ok(())
    }

    #[tokio::test]
    async fn absent_branch_never_queries_github() {
        let cache = Arc::new(GitCache::new(FakeGit::new(Some(42))));
        *cache.provider.branch.lock() = None;
        assert_eq!(cache.context(Path::new("/project")).await, None);
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 0);
        assert!(cache.entries.lock().is_empty());
    }

    #[tokio::test]
    async fn github_concurrency_is_bounded() -> eyre::Result<()> {
        let cache = Arc::new(GitCache::new(FakeGit::new(None)));
        for directory in 0..10 {
            cache
                .context(Path::new(&format!("/project/{directory}")))
                .await;
        }
        cache.provider.started.acquire_many(4).await?.forget();
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 4);
        finish_refresh(&cache).await?;
        assert_eq!(cache.provider.calls.load(Ordering::SeqCst), 4);
        Ok(())
    }

    #[tokio::test]
    async fn git_reads_the_live_branch_and_ignores_nonrepositories() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        assert_eq!(Commands.branch(directory.path()).await, None);
        let initialized = Command::new("git")
            .args(["init", "--quiet", "--initial-branch=first"])
            .arg(directory.path())
            .status()
            .await?;
        assert!(initialized.success());
        assert_eq!(
            Commands.branch(directory.path()).await.as_deref(),
            Some("first")
        );

        // A worktree has a .git file; let Git resolve it instead of parsing its internals.
        let linked = tempfile::tempdir()?;
        std::fs::write(
            linked.path().join(".git"),
            format!("gitdir: {}\n", directory.path().join(".git").display()),
        )?;
        assert_eq!(
            Commands.branch(linked.path()).await.as_deref(),
            Some("first")
        );
        std::fs::write(
            directory.path().join(".git/HEAD"),
            "ref: refs/heads/second\n",
        )?;
        assert_eq!(
            Commands.branch(linked.path()).await.as_deref(),
            Some("second")
        );
        std::fs::write(
            directory.path().join(".git/HEAD"),
            "0000000000000000000000000000000000000001\n",
        )?;
        assert_eq!(Commands.branch(linked.path()).await, None);
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_commands_are_optional() {
        assert_eq!(
            command_output(
                &mut Command::new("micromux-missing-command-test"),
                GIT_TIMEOUT
            )
            .await,
            None
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_and_slow_commands_are_optional() {
        assert_eq!(
            command_output(&mut Command::new("false"), GIT_TIMEOUT).await,
            None
        );
        assert_eq!(
            command_output(Command::new("sleep").arg("10"), Duration::from_millis(10)).await,
            None
        );
    }
}
