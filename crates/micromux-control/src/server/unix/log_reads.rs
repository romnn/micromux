use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;

use crate::{DiskLogReadHealth, ErrorCode, Response};

const WORKER_COUNT: usize = 4;
const QUEUE_CAPACITY: usize = 4;
/// Disk-read deadline, leaving the connection timeout time to encode a typed busy response.
const READ_TIMEOUT: Duration = Duration::from_secs(8);

type Read = Box<dyn FnOnce() -> Response + Send + 'static>;

struct Job {
    read: Read,
    response: tokio::sync::oneshot::Sender<Response>,
    queue_permit: QueuePermit,
    state: Arc<JobState>,
}

/// Keeps uncancellable filesystem reads off Tokio workers while bounding blocked threads and queued
/// work after clients time out.
///
/// Workers still executing after their callers leave are not replaced: the underlying filesystem
/// call cannot be cancelled, so replacement would turn a stalled mount into an unbounded thread
/// leak. Saturation instead returns the pool's health counters to the caller and emits them through
/// tracing.
struct BlockingLogReadPool {
    jobs: mpsc::SyncSender<Job>,
    health: Arc<PoolHealth>,
    worker_count: usize,
    queue_capacity: usize,
    read_timeout: Duration,
}

#[derive(Default)]
struct PoolHealth {
    active: AtomicUsize,
    queued: AtomicUsize,
    abandoned: AtomicUsize,
    timed_out_requests: AtomicUsize,
}

const JOB_QUEUED: u8 = 0;
const JOB_ACTIVE: u8 = 1;
const JOB_ABANDONED: u8 = 2;
const JOB_COMPLETE: u8 = 3;
const JOB_CANCELED: u8 = 4;

struct JobState {
    state: AtomicU8,
    health: Arc<PoolHealth>,
}

impl JobState {
    fn new(health: Arc<PoolHealth>) -> Self {
        Self {
            state: AtomicU8::new(JOB_QUEUED),
            health,
        }
    }

    fn caller_left(&self) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            let next = match state {
                JOB_QUEUED => JOB_CANCELED,
                JOB_ACTIVE => JOB_ABANDONED,
                _ => return,
            };
            if self
                .state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                if next == JOB_ABANDONED {
                    self.health.abandoned.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        }
    }
}

/// Records queued cancellation or an in-flight abandoned read when its request future ends.
struct RequestGuard {
    state: Arc<JobState>,
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.state.caller_left();
    }
}

struct QueuePermit {
    health: Arc<PoolHealth>,
}

impl Drop for QueuePermit {
    fn drop(&mut self) {
        self.health.queued.fetch_sub(1, Ordering::AcqRel);
    }
}

impl BlockingLogReadPool {
    fn new(worker_count: usize, queue_capacity: usize) -> std::io::Result<Self> {
        Self::with_timeout(worker_count, queue_capacity, READ_TIMEOUT)
    }

    fn with_timeout(
        worker_count: usize,
        queue_capacity: usize,
        read_timeout: Duration,
    ) -> std::io::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<Job>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let health = Arc::new(PoolHealth::default());
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let health = Arc::clone(&health);
            std::thread::Builder::new()
                .name(format!("micromux-log-read-{index}"))
                .spawn(move || worker(&receiver, &health))?;
        }
        Ok(Self {
            jobs,
            health,
            worker_count,
            queue_capacity,
            read_timeout,
        })
    }

    async fn run(&self, read: impl FnOnce() -> Response + Send + 'static) -> Response {
        if self.all_workers_abandoned() {
            return self.saturated_response();
        }
        let (response, wait) = tokio::sync::oneshot::channel();
        let Some(queue_permit) = self.reserve_queue_slot() else {
            return self.saturated_response();
        };
        let state = Arc::new(JobState::new(Arc::clone(&self.health)));
        let _request = RequestGuard {
            state: Arc::clone(&state),
        };
        match self.jobs.try_send(Job {
            read: Box::new(read),
            response,
            queue_permit,
            state,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => return self.saturated_response(),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Response::error(ErrorCode::Internal, "the disk log reader is unavailable");
            }
        }
        match tokio::time::timeout(self.read_timeout, wait).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => Response::error(
                ErrorCode::Internal,
                "the disk log reader stopped unexpectedly",
            ),
            Err(_) => {
                self.health
                    .timed_out_requests
                    .fetch_add(1, Ordering::Relaxed);
                Response::error(
                    ErrorCode::Busy,
                    format!(
                        "the disk log read exceeded {:.1}s; retry the request",
                        self.read_timeout.as_secs_f64()
                    ),
                )
            }
        }
    }

    fn all_workers_abandoned(&self) -> bool {
        self.health.abandoned.load(Ordering::Relaxed) >= self.worker_count
    }

    fn reserve_queue_slot(&self) -> Option<QueuePermit> {
        self.health
            .queued
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |queued| {
                (queued < self.queue_capacity).then_some(queued + 1)
            })
            .ok()
            .map(|_| QueuePermit {
                health: Arc::clone(&self.health),
            })
    }

    fn saturated_response(&self) -> Response {
        let health = self.health();
        tracing::warn!(
            active = health.active,
            queued = health.queued,
            abandoned = health.abandoned,
            timed_out_requests = health.timed_out_requests,
            workers = health.workers,
            queue_capacity = health.queue_capacity,
            "disk log reader pool is saturated"
        );
        Response::error(
            ErrorCode::Busy,
            format!(
                "the disk log reader is busy ({}/{} workers active, {}/{} jobs queued); retry the \
                 request ({} abandoned reads still running)",
                health.active,
                health.workers,
                health.queued,
                health.queue_capacity,
                health.abandoned
            ),
        )
    }

    fn health(&self) -> DiskLogReadHealth {
        let active = self.health.active.load(Ordering::Relaxed);
        let abandoned = self.health.abandoned.load(Ordering::Relaxed);
        DiskLogReadHealth {
            initialized: true,
            available: active.max(abandoned) < self.worker_count,
            workers: self.worker_count,
            active,
            queue_capacity: self.queue_capacity,
            queued: self.health.queued.load(Ordering::Relaxed),
            abandoned,
            timed_out_requests: self.health.timed_out_requests.load(Ordering::Relaxed),
        }
    }
}

fn worker(receiver: &Mutex<mpsc::Receiver<Job>>, health: &PoolHealth) {
    loop {
        let job = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(poisoned) => poisoned.into_inner().recv(),
        };
        let Ok(job) = job else {
            return;
        };
        let Job {
            read,
            response,
            queue_permit,
            state,
        } = job;
        drop(queue_permit);
        if state
            .state
            .compare_exchange(JOB_QUEUED, JOB_ACTIVE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            continue;
        }
        health.active.fetch_add(1, Ordering::Relaxed);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)).unwrap_or_else(|_| {
                Response::error(ErrorCode::Internal, "the disk log reader panicked")
            });
        let previous = state.state.swap(JOB_COMPLETE, Ordering::AcqRel);
        health.active.fetch_sub(1, Ordering::Relaxed);
        if previous == JOB_ABANDONED {
            health.abandoned.fetch_sub(1, Ordering::Relaxed);
        }
        let _ = response.send(result);
    }
}

#[derive(Default)]
struct PoolRegistry {
    state: Mutex<PoolRegistryState>,
}

#[derive(Default)]
struct PoolRegistryState {
    pool: Option<Arc<BlockingLogReadPool>>,
    /// Whether the latest lazy start attempt failed.
    ///
    /// Health inspection reports the failure but never retries it because `Describe` must not spawn
    /// workers. The next real disk read retries initialization.
    last_start_failed: bool,
}

impl PoolRegistry {
    fn get_or_start(&self) -> Result<Arc<BlockingLogReadPool>, String> {
        self.get_or_start_with(|| BlockingLogReadPool::new(WORKER_COUNT, QUEUE_CAPACITY))
    }

    fn get_or_start_with(
        &self,
        start: impl FnOnce() -> std::io::Result<BlockingLogReadPool>,
    ) -> Result<Arc<BlockingLogReadPool>, String> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pool) = &state.pool {
            return Ok(Arc::clone(pool));
        }

        match start() {
            Ok(pool) => {
                let pool = Arc::new(pool);
                state.pool = Some(Arc::clone(&pool));
                state.last_start_failed = false;
                Ok(pool)
            }
            Err(err) => {
                state.last_start_failed = true;
                Err(err.to_string())
            }
        }
    }

    fn health(&self, worker_count: usize, queue_capacity: usize) -> DiskLogReadHealth {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pool.as_ref().map_or_else(
            || DiskLogReadHealth {
                initialized: false,
                available: !state.last_start_failed,
                workers: worker_count,
                active: 0,
                queue_capacity,
                queued: 0,
                abandoned: 0,
                timed_out_requests: 0,
            },
            |pool| pool.health(),
        )
    }
}

static DISK_LOG_READS: LazyLock<PoolRegistry> = LazyLock::new(PoolRegistry::default);

pub(super) async fn run(read: impl FnOnce() -> Response + Send + 'static) -> Response {
    match DISK_LOG_READS.get_or_start() {
        Ok(pool) => pool.run(read).await,
        Err(err) => Response::error(
            ErrorCode::Internal,
            format!("failed to start disk log readers: {err}"),
        ),
    }
}

pub(super) fn health() -> DiskLogReadHealth {
    DISK_LOG_READS.health(WORKER_COUNT, QUEUE_CAPACITY)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    #[tokio::test]
    async fn pool_reuses_its_worker_thread_across_reads() -> eyre::Result<()> {
        let pool = BlockingLogReadPool::new(1, 1)?;
        let threads = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let threads = Arc::clone(&threads);
            let response = pool
                .run(move || {
                    threads
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(std::thread::current().id());
                    Response::ShuttingDown
                })
                .await;
            assert_matches!(response, Response::ShuttingDown);
        }

        let threads = threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads.first(), threads.get(1));
        Ok(())
    }

    #[tokio::test]
    async fn saturated_pool_reports_active_and_queued_work() -> eyre::Result<()> {
        let pool = Arc::new(BlockingLogReadPool::new(1, 1)?);
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let first = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move {
                pool.run(move || {
                    let _ = started.send(());
                    let _ = release_rx.recv();
                    Response::ShuttingDown
                })
                .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv()).await??;
        let second = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.run(|| Response::ShuttingDown).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while (pool.health().active, pool.health().queued) != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        // A healthy busy worker must still accept bounded queued work; only abandoned workers make
        // the pool unusable.
        assert!(!pool.health().available);
        let rejected = pool.run(|| Response::ShuttingDown).await;

        assert_matches!(
            rejected,
            Response::Error {
                code: ErrorCode::Busy,
                message,
            } if message.contains("1/1 workers active") && message.contains("1/1 jobs queued")
        );
        release.send(())?;
        assert_matches!(first.await?, Response::ShuttingDown);
        assert_matches!(second.await?, Response::ShuttingDown);
        Ok(())
    }

    #[tokio::test]
    async fn canceled_queued_read_does_not_consume_a_worker() -> eyre::Result<()> {
        let pool = Arc::new(BlockingLogReadPool::new(1, 1)?);
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let first = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move {
                pool.run(move || {
                    let _ = started.send(());
                    let _ = release_rx.recv();
                    Response::ShuttingDown
                })
                .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv()).await??;

        let stale_executions = Arc::new(AtomicUsize::new(0));
        let stale = tokio::spawn({
            let pool = Arc::clone(&pool);
            let stale_executions = Arc::clone(&stale_executions);
            async move {
                pool.run(move || {
                    stale_executions.fetch_add(1, Ordering::Relaxed);
                    Response::ShuttingDown
                })
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while (pool.health().active, pool.health().queued) != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        stale.abort();
        let _ = stale.await;

        release.send(())?;
        assert_matches!(first.await?, Response::ShuttingDown);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while (pool.health().active, pool.health().queued) != (0, 0) {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            pool.run(|| Response::ShuttingDown),
        )
        .await?;

        assert_matches!(response, Response::ShuttingDown);
        assert_eq!(stale_executions.load(Ordering::Relaxed), 0);
        Ok(())
    }

    #[tokio::test]
    async fn stalled_read_times_out_without_replacing_its_worker() -> eyre::Result<()> {
        let timeout = Duration::from_millis(30);
        let pool = Arc::new(BlockingLogReadPool::with_timeout(1, 1, timeout)?);
        let (started, started_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let request = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move {
                pool.run(move || {
                    let _ = started.send(());
                    let _ = release_rx.recv();
                    Response::ShuttingDown
                })
                .await
            }
        });
        tokio::task::spawn_blocking(move || started_rx.recv()).await??;
        let response = request.await?;

        assert_matches!(
            response,
            Response::Error {
                code: ErrorCode::Busy,
                message,
            } if message.contains("exceeded")
        );
        let health = pool.health();
        assert_eq!(health.active, 1);
        assert_eq!(health.workers, 1);
        assert_eq!(health.abandoned, 1);
        assert_eq!(health.timed_out_requests, 1);
        assert!(!health.available);
        let rejected = pool.run(|| Response::ShuttingDown).await;
        assert_matches!(
            rejected,
            Response::Error {
                code: ErrorCode::Busy,
                message,
            } if message.contains("1 abandoned")
        );
        assert_eq!(pool.health().queued, 0);

        release.send(())?;
        tokio::time::timeout(Duration::from_secs(1), async {
            while pool.health().active != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert_eq!(pool.health().abandoned, 0);
        assert!(pool.health().available);
        Ok(())
    }

    #[test]
    fn pool_start_failure_is_retryable_and_health_does_not_start_workers() -> eyre::Result<()> {
        let registry = PoolRegistry::default();

        let health = registry.health(1, 1);
        assert!(!health.initialized);
        assert!(health.available);
        assert_eq!(health.workers, 1);
        let Err(message) =
            registry.get_or_start_with(|| Err(std::io::Error::other("injected spawn failure")))
        else {
            eyre::bail!("injected pool startup failure unexpectedly succeeded");
        };
        assert!(message.contains("injected spawn failure"));
        let failed = registry.health(1, 1);
        assert!(!failed.initialized);
        assert!(!failed.available);
        assert_eq!(failed.workers, 1);

        let pool = registry
            .get_or_start_with(|| BlockingLogReadPool::new(1, 1))
            .map_err(eyre::Report::msg)?;
        assert!(Arc::ptr_eq(
            &pool,
            &registry
                .get_or_start_with(|| {
                    Err(std::io::Error::other("initializer must not run twice"))
                })
                .map_err(eyre::Report::msg)?
        ));
        let health = registry.health(1, 1);
        assert!(health.initialized);
        assert!(health.available);
        Ok(())
    }
}
