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
struct BlockingLogReadPool {
    jobs: mpsc::SyncSender<Job>,
}

impl BlockingLogReadPool {
    fn new(worker_count: usize, queue_capacity: usize) -> std::io::Result<Self> {
        let (jobs, receiver) = mpsc::sync_channel::<Job>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("micromux-log-read-{index}"))
                .spawn(move || worker(&receiver))?;
        }
        Ok(Self { jobs })
    }

    async fn run(&self, read: impl FnOnce() -> Response + Send + 'static) -> Response {
        let (response, wait) = tokio::sync::oneshot::channel();
        match self.jobs.try_send(Job {
            read: Box::new(read),
            response,
        }) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Response::error(
                    ErrorCode::LimitExceeded,
                    "the disk log reader is busy; retry the request",
                );
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Response::error(ErrorCode::Internal, "the disk log reader is unavailable");
            }
        }
        wait.await
            .unwrap_or_else(|_| Response::error(ErrorCode::Internal, "the disk log reader stopped"))
    }
}

fn worker(receiver: &Mutex<mpsc::Receiver<Job>>) {
    loop {
        let job = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(poisoned) => poisoned.into_inner().recv(),
        };
        let Ok(job) = job else {
            return;
        };
        let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job.read))
            .unwrap_or_else(|_| {
                Response::error(ErrorCode::Internal, "the disk log reader panicked")
            });
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
            assert!(matches!(response, Response::ShuttingDown));
        }

        let threads = threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads.first(), threads.get(1));
        Ok(())
    }
}
