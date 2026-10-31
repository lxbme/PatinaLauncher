//! The app's tokio runtime, for the core library's async work and its
//! blocking calls (`spawn_blocking`); never the gpui main thread.
//!
//! `main` owns it and drops it only after the gpui app has returned.

/// Worker threads: downloads are IO-bound and the core bounds its own
/// concurrency, so a few workers are enough.
pub const WORKER_THREADS: usize = 4;
pub const THREAD_NAME: &str = "patina-rt";

pub fn start() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .thread_name(THREAD_NAME)
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread_name() -> Option<String> {
        std::thread::current().name().map(str::to_owned)
    }

    #[test]
    fn runtime_threads_are_named() {
        let rt = start();
        let (worker, blocking) = rt.block_on(async {
            let worker = tokio::spawn(async { thread_name() }).await.unwrap();
            let blocking = tokio::task::spawn_blocking(thread_name).await.unwrap();
            (worker, blocking)
        });
        assert_eq!(worker.as_deref(), Some(THREAD_NAME));
        assert_eq!(blocking.as_deref(), Some(THREAD_NAME));
    }
}
