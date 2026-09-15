//! Always-on decoded-stream capture of running pipelines (T-092, ADR-0011 §7, stream contract
//! §14.7) and the recorded-capture form of the `inspector` opener (§14.8).
//!
//! - **Recording.** Every inspector output stream a pipeline offers (at start and for outputs a
//!   hot edit adds) is teed into the run's [`DecodedCaptures`] store before its graph publishes,
//!   as a local consumer of the stream's publisher: the store's writer runs on that consumer's
//!   writer thread behind a bounded ring, so a slow disk drops and counts records and the
//!   pipeline thread never waits (see `hk_store::decoded`).
//! - **Replay** `open/inspector?capture=<id>[&from_frame=<n>][&field_map=<recipe>@<version>:<map>]`:
//!   the stored frame records from frame `n` (index seek), re-parsed with the named field map
//!   when given (layers added, `metadata.fit` set), under the recording's class (the publisher's
//!   §6 gate runs again, fail closed). Header `inspector.source` is
//!   `{kind: capture, capture_id, reparse}`; stream id `capture/<id>`; `seq` is the replay
//!   stream's own, `t`/`metadata`/`content` the recording's. Records are paced to the
//!   consumer (never dropped for speed); the stream finishes at the end of what was stored when
//!   it opened. Only frame records are replayed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use hk_recipe::fields::eval::Evaluator;
use hk_store::decoded::{CaptureQuota, DecodedCaptures, is_capture_id};
use hk_stream::inspector::{CaptureSource, InspectorSource, RecordedFrames};
use hk_stream::{OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig};

use crate::recipes::runtime::{RecipeRuntime, StreamEntry};

/// How long a replay waits for its consumer to subscribe.
const SUBSCRIBE_WAIT: Duration = Duration::from_secs(10);

/// Tees the `inspector` entries into the runtime's capture store (no store: nothing).
pub(crate) fn tee_streams<'a>(
    rt: &RecipeRuntime,
    entries: impl IntoIterator<Item = &'a StreamEntry>,
) {
    let Some(store) = rt.captures.get() else {
        return;
    };
    for e in entries {
        if e.kind == "inspector" {
            // A refused subscription (consumer cap) leaves the stream unrecorded, never failing
            // the pipeline.
            let _ = store.tee(&e.header, &e.handle);
        }
    }
}

impl RecipeRuntime {
    /// Sets the decoded-capture store later pipelines and edits record into (once; `false` when
    /// one was already set).
    pub fn set_capture_store(&self, store: DecodedCaptures) -> bool {
        self.captures.set(store).is_ok()
    }

    /// The decoded-capture store, when one is set.
    pub fn capture_store(&self) -> Option<&DecodedCaptures> {
        self.captures.get()
    }

    /// Opens the default store at `<data_dir>/captures` with [`CaptureQuota::from_env`] unless
    /// one is set. A store that cannot be opened leaves pipelines unrecorded.
    pub fn attach_default_capture_store(&self, data_dir: &Path) {
        if self.captures.get().is_none()
            && let Ok(store) =
                DecodedCaptures::open(data_dir.join("captures"), CaptureQuota::from_env())
        {
            let _ = self.captures.set(store);
        }
    }

    /// `open/inspector?capture=…` (see the module docs).
    pub fn open_capture(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let id = req
            .param("capture")
            .filter(|v| is_capture_id(v))
            .ok_or_else(|| OpenRefusal::new(400, "bad-request", "give capture=<id>"))?;
        let from = match req.param("from_frame") {
            None => 0,
            Some(v) => v.parse::<u64>().map_err(|_| {
                OpenRefusal::new(400, "bad-request", "from_frame is a non-negative integer")
            })?,
        };
        let ev = match req.param("field_map") {
            None => None,
            Some(spec) => Some(self.field_map_evaluator(spec)?),
        };
        let store = self
            .captures
            .get()
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no capture store on this run"))?;
        let cursor = store
            .open_at(id, from)
            .map_err(|_| OpenRefusal::new(500, "unreadable", "the capture could not be opened"))?
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such capture"))?;
        let mut frames = RecordedFrames::open(cursor.reader).map_err(|_| {
            OpenRefusal::new(
                422,
                "unreadable",
                "the capture is not a readable inspector stream",
            )
        })?;
        let mut header = frames.header().clone();
        header.stream_id = format!("capture/{id}");
        if let Some(p) = header.inspector.as_mut() {
            p.source = InspectorSource::Capture {
                capture_id: id.to_owned(),
                reparse: ev.is_some(),
            };
        }
        let config = PublisherConfig::default();
        let mut publisher = Publisher::new(header.clone(), config)
            .map_err(|_| OpenRefusal::new(500, "failed", "creating the replay stream"))?;
        let handle = publisher.handle();
        let closed = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&closed);
        let pace = handle.clone();
        let high_water = (config.queue_bytes / 2) as u64;
        thread::Builder::new()
            .name("hk-capture-replay".into())
            .spawn(move || {
                let deadline = Instant::now() + SUBSCRIBE_WAIT;
                while pace.open_consumers() == 0 {
                    if stop.load(Ordering::SeqCst) || Instant::now() > deadline {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                while let Ok(Some(mut rec)) = frames.next_frame() {
                    loop {
                        if stop.load(Ordering::SeqCst) || pace.open_consumers() == 0 {
                            return;
                        }
                        let queued = pace
                            .consumer_stats()
                            .iter()
                            .map(|s| s.queued_bytes)
                            .max()
                            .unwrap_or(0);
                        if queued <= high_water {
                            break;
                        }
                        thread::sleep(Duration::from_millis(2));
                    }
                    if let Some(ev) = &ev
                        && let Some(tree) = ev.eval_record(&rec)
                        && let Some(content) = rec.content.as_mut()
                    {
                        rec.metadata.fit = Some(tree.fit);
                        content.layers = Some(tree);
                    }
                    // A gated or refused record is the gate's decision; keep replaying.
                    let _ = publisher.publish_frame(&rec);
                }
                drop(publisher);
            })
            .map_err(|_| OpenRefusal::new(500, "failed", "starting the replay"))?;
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(CloseOnDrop(closed)),
        })
    }

    /// `<recipe_id>@<version>:<map_id>` → the compiled field map.
    fn field_map_evaluator(&self, spec: &str) -> Result<Evaluator, OpenRefusal> {
        let bad = || {
            OpenRefusal::new(
                400,
                "bad-request",
                "field_map is <recipe_id>@<version>:<map_id>",
            )
        };
        let (recipe, map) = spec.split_once(':').ok_or_else(bad)?;
        let (rid, ver) = recipe.split_once('@').ok_or_else(bad)?;
        let ver: u32 = ver.parse().map_err(|_| bad())?;
        let recipe = self
            .store()
            .get(rid, Some(ver))
            .map_err(|_| OpenRefusal::new(404, "not-found", "no such recipe version"))?;
        let map = recipe
            .field_maps
            .get(map)
            .ok_or_else(|| OpenRefusal::new(404, "not-found", "no such field map"))?;
        Evaluator::new(map).map_err(|_| {
            OpenRefusal::new(422, "invalid", "the recipe's field map does not compile")
        })
    }
}

struct CloseOnDrop(Arc<AtomicBool>);

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
