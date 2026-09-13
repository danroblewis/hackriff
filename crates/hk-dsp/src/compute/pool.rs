//! The shared compute thread pool for the multi-threaded CPU provider.
//!
//! One rayon pool per process, built on first use. Its size comes from the first caller's
//! request, else `HK_COMPUTE_THREADS`, else the machine's available parallelism. Stages that
//! run on their own threads (ring readers) submit batches into it; rayon queues the work, so
//! concurrent stages share the cores instead of oversubscribing them.

use std::sync::{Arc, OnceLock};

use rayon::{ThreadPool, ThreadPoolBuilder};

static POOL: OnceLock<Result<Arc<ThreadPool>, String>> = OnceLock::new();

/// The shared pool, creating it with `threads` workers (or the default) on first use.
/// Later calls return the existing pool whatever `threads` they pass.
pub fn shared(threads: Option<usize>) -> Result<Arc<ThreadPool>, String> {
    POOL.get_or_init(|| {
        let n = threads
            .or_else(|| {
                std::env::var("HK_COMPUTE_THREADS")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
            })
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
        ThreadPoolBuilder::new()
            .num_threads(n)
            .thread_name(|i| format!("hk-compute-{i}"))
            .build()
            .map(Arc::new)
            .map_err(|e| format!("rayon pool: {e}"))
    })
    .clone()
}
