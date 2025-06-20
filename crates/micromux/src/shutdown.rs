use std::future::Future;
use tokio::{
    signal,
    sync::{broadcast, watch},
};

/// Register signal handlers for ctrl+C and SIGTERM
///
/// # Panics
/// When running on unix-like operating systems and the SIGTERM signal handler cannot be registered.
fn register_handlers() -> impl Future<Output = ()> {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install termination signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    async {
        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }

        tracing::warn!("received shutdown signal");
    }
}

/// A handle to the shutdown.
///
/// ```rust
/// # tokio_test::block_on(async {
/// let shutdown = shutdown::Shutdown::<()>::new();
/// tokio::spawn({
///   let shutdown = shutdown.clone();
///   async move {
///     // send shutdown signal after one second
///     tokio::time::sleep(std::time::Duration::from_secs(1));
///     shutdown.shutdown(())
///   }
/// });
/// // wait for shutdown
/// shutdown.handle().changed().await.expect("receive shutdown");
/// # })
/// ```
pub type Handle = broadcast::Receiver<()>;
// pub type Handle = watch::Receiver<()>;

/// A shutdown handler for an application. It can be cloned cheaply wherever needed.
///
/// New handles can be created with the [`handle`](Self::handle) function, which creates futures
/// that will complete once a shutdown signal is received.
#[derive(Clone)]
pub struct Shutdown {
    sender: broadcast::Sender<()>,
    // receiver: broadcast::Receiver<()>,
}

impl Shutdown {
    /// Create a new shutdown handle. This can only be called once per application instance.
    ///
    /// Signal handles can only be registered once for the duration of the entire process and
    /// creating another shutdown handler would break the previous one without notice.
    ///
    /// # Errors
    ///
    /// If this function is called more than once during the lifetime of a process, an error will be
    /// returned.
    pub fn new() -> Self {
        // let (tx, rx) = watch::channel(());
        let (tx, rx) = broadcast::channel(1);
        let handle = register_handlers();

        tokio::spawn({
            let tx = tx.clone();
            async move {
                handle.await;
                tx.send(()).ok();
            }
        });

        Self {
            sender: tx,
            // receiver: rx,
        }
    }

    pub fn shutdown(&self) {
        self.sender.send(()).expect("can send shutdown signal");
    }

    pub fn handle(&self) -> Handle {
        self.sender.subscribe()
    }
}
