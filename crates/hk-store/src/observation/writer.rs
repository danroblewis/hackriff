//! The bounded producer queue and the writer thread.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use hk_model::attention::observation::ObservationRecord;

use super::log::{ObservationLogStats, ObservationStore};

/// Called on the writer thread for every record it appends (e.g. the `observations` stream).
pub type RecordTap = Box<dyn FnMut(&ObservationRecord) + Send>;

enum Msg {
    Record(ObservationRecord),
    Stop,
}

/// The producers' end: `offer` never blocks (a full queue drops and counts).
#[derive(Clone)]
pub struct ObservationQueue {
    tx: SyncSender<Msg>,
    stats: Arc<ObservationLogStats>,
}

impl ObservationQueue {
    /// Offers a record; `false` when it was dropped (queue full or writer gone).
    pub fn offer(&self, rec: ObservationRecord) -> bool {
        self.stats.offered.fetch_add(1, Ordering::Relaxed);
        match self.tx.try_send(Msg::Record(rec)) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Counters shared with the store.
    pub fn stats(&self) -> &Arc<ObservationLogStats> {
        &self.stats
    }
}

/// The writer thread over an [`ObservationStore`]. Dropping it (or [`ObservationWriter::finish`])
/// drains the queue, flushes and fsyncs.
pub struct ObservationWriter {
    queue: ObservationQueue,
    store: ObservationStore,
    thread: Option<JoinHandle<()>>,
}

impl ObservationWriter {
    /// Starts the writer thread with a queue of the store's `queue_len`.
    pub fn spawn(store: ObservationStore, tap: Option<RecordTap>) -> io::Result<Self> {
        let cap = store.config().queue_len.max(1);
        let (tx, rx) = sync_channel(cap);
        let s = store.clone();
        let thread = std::thread::Builder::new()
            .name("hk-observations".into())
            .spawn(move || run(&s, &rx, tap))?;
        Ok(Self {
            queue: ObservationQueue {
                tx,
                stats: Arc::clone(store.stats()),
            },
            store,
            thread: Some(thread),
        })
    }

    /// A producer handle.
    pub fn queue(&self) -> ObservationQueue {
        self.queue.clone()
    }

    /// The store (queries).
    pub fn store(&self) -> &ObservationStore {
        &self.store
    }

    /// Drains, flushes, fsyncs and joins the thread.
    pub fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(t) = self.thread.take() {
            // Blocking send: the thread is draining, so a full queue frees up.
            let _ = self.queue.tx.send(Msg::Stop);
            let _ = t.join();
        }
    }
}

impl Drop for ObservationWriter {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(store: &ObservationStore, rx: &Receiver<Msg>, mut tap: Option<RecordTap>) {
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Msg::Record(rec)) => {
                store.append(&rec);
                if let Some(t) = tap.as_mut() {
                    t(&rec);
                }
                store.maybe_flush();
            }
            Err(RecvTimeoutError::Timeout) => store.maybe_flush(),
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    store.seal();
}
