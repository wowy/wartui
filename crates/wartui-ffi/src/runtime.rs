//! The one tokio runtime the library owns.
//!
//! A phone calls in from its own threads, and from Kotlin's or Swift's executors, none of which is
//! a tokio runtime. So every task the library starts runs here: the transports, the capture and
//! the view task, and the work an exported `async fn` hands over. An exported `async fn` awaits
//! only `tokio::sync` primitives or a `JoinHandle` of a task spawned here, which any executor can
//! poll. Nothing calls `block_on`, so no call from a phone's thread waits on this runtime's
//! workers.
//!
//! Two workers: the capture and the transport each keep one busy at most, and a phone has cores
//! to spare for little else. It is built on first use and never shut down, since a task left
//! running when a library unloads has nothing to return to.

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

/// The library's runtime, built on first use.
///
/// # Panics
/// When the OS will not start the runtime's threads, which leaves the library nothing to run on.
pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wartui")
            .enable_time()
            .build()
            .expect("the OS starts the library's runtime threads")
    })
}

/// Run `work` on the library's runtime. Awaiting the handle needs no runtime of its own.
pub(crate) fn spawn<F>(work: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    runtime().spawn(work)
}
