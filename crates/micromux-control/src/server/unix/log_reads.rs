use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};

use crate::{ErrorCode, Response};

const WORKER_COUNT: usize = 4;
const QUEUE_CAPACITY: usize = 4;

type Read = Box<dyn FnOnce() -> Response + Send + 'static>;

struct Job {
    read: Read,
    response: tokio::sync::oneshot::Sender<Response>,
}

/// Keeps uncancellable filesystem reads off Tokio workers while bounding blocked threads and queued
/// work after clients time out.
///
/// Workers whose clients time out are not replaced: the underlying filesystem call cannot be
/// cancelled, so replacement would turn a stalled mount into an unbounded thread leak. Saturation
/// instead returns the active and queued counts to the caller and emits them through tracing.
struct BlockingLogReadPool {
    jobs: mpsc::SyncSender<Job>,
    health: Arc<PoolHealth>,
    worker_count: usize,
    queue_capacity: usize,
}

#[derive(Default)]
struct PoolHealth {
    active: AtomicUsize,
    queued: AtomicUsize,
}

impl BlockingLogReadPool {
    fn new(worker_count: usize, queue_capacity: usize) -> std::io::Result<Self> {
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
        })
    }

    async fn run(&self, read: impl FnOnce() -> Response + Send + 'static) -> Response {
        let (response, wait) = tokio::sync::oneshot::channel();
        self.health.queued.fetch_add(1, Ordering::Relaxed);
        match self.jobs.try_send(Job {
            read: Box::new(read),
            response,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                self.health.queued.fetch_sub(1, Ordering::Relaxed);
                let (active, queued) = self.health();
                tracing::warn!(
                    active,
                    queued,
                    workers = self.worker_count,
                    queue_capacity = self.queue_capacity,
                    "disk log reader pool is saturated"
                );
                return Response::error(
                    ErrorCode::LimitExceeded,
                    format!(
                        "the disk log reader is busy ({active}/{} workers active, {queued}/{} jobs \
                         queued); retry the request",
                        self.worker_count, self.queue_capacity
                    ),
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.health.queued.fetch_sub(1, Ordering::Relaxed);
                return Response::error(ErrorCode::Internal, "the disk log reader is unavailable");
            }
        }
        wait.await
            .unwrap_or_else(|_| Response::error(ErrorCode::Internal, "the disk log reader stopped"))
    }

    fn health(&self) -> (usize, usize) {
        (
            self.health.active.load(Ordering::Relaxed),
            self.health.queued.load(Ordering::Relaxed),
        )
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
        health.queued.fetch_sub(1, Ordering::Relaxed);
        if job.response.is_closed() {
            continue;
        }
        health.active.fetch_add(1, Ordering::Relaxed);
        let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.read))
            .unwrap_or_else(|_| {
                Response::error(ErrorCode::Internal, "the disk log reader panicked")
            });
        health.active.fetch_sub(1, Ordering::Relaxed);
        let _ = job.response.send(response);
    }
}

static DISK_LOG_READS: LazyLock<Result<BlockingLogReadPool, String>> = LazyLock::new(|| {
    BlockingLogReadPool::new(WORKER_COUNT, QUEUE_CAPACITY).map_err(|err| err.to_string())
});

pub(super) async fn run(read: impl FnOnce() -> Response + Send + 'static) -> Response {
    match &*DISK_LOG_READS {
        Ok(pool) => pool.run(read).await,
        Err(err) => Response::error(
            ErrorCode::Internal,
            format!("failed to start disk log readers: {err}"),
        ),
    }
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
            while pool.health() != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await?;

        let rejected = pool.run(|| Response::ShuttingDown).await;

        assert_matches!(
            rejected,
            Response::Error {
                code: ErrorCode::LimitExceeded,
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
            while pool.health() != (1, 1) {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        stale.abort();
        let _ = stale.await;

        release.send(())?;
        assert_matches!(first.await?, Response::ShuttingDown);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while pool.health() != (0, 0) {
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
}
