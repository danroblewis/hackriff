//! Recipe `messages` outputs (ADR-0011 §2.2, §5; T-111): parsed frames → Decode rows in the
//! Repository, through the same ingestion as plugin decodes.
//!
//! - **Mapping (declared, never per protocol).** The output's `decode` mapping names the
//!   `frame_model` (message type), an `identity` field and scheme (the Emitter identity, e.g.
//!   ICAO, PI, RIC), `metadata` field paths (the always-stored summary) and `content` field paths;
//!   [`RowSpec::decode`] reads them from a frame's layer tree. Keys are the path's last segment,
//!   or the whole dotted path when two mapped paths of the same list share it.
//! - **Which frames.** Only CRC-valid frames whose field map fit (`ok` or `partial`) and that
//!   carry every `require`d field (by default: any mapped field) become rows. There is no
//!   deduplication, as for plugin decodes: every frame is a row, and entity resolution folds the
//!   repeated identities into one emitter (`Repository::record_sighting`, keyed by decode id).
//! - **Real-time path.** The pipeline thread only filters a frame and `try_send`s its time,
//!   channel and `Arc` layer tree onto a bounded queue ([`MESSAGE_QUEUE`]); a full queue drops the
//!   frame and counts `decodes_dropped`. Nothing on it allocates, blocks or touches SQLite.
//! - **Writer.** One thread per output owns its own repository connection (SQLite WAL) inside a
//!   [`hk_plugins::Ingest`], exactly as a plugin chain does: the decode is sanitised by the
//!   recipe's `output_policy` (the §9.3 policy shape, parsed by the manifest rules) under the
//!   pipeline's effective class, then `Ingest::store_decode` applies the repository content gate,
//!   the identity sighting (attached to the target emitter as context) and the republish on
//!   `decodes/<pipeline>/<output>`. New emitters get the family step
//!   ([`crate::chains::plugin::classify_decoder_emitters`]) with the mapping's `service` (or the
//!   recipe id) as decoder evidence.
//! - **A vote bar on weak identities (T-962).** A scheme whose check cannot carry an identity on
//!   one frame ([`hk_model::IdentityScheme::commit_votes`] > 1; today `rds-pi`, whose block check
//!   is 10 bits) is counted per writer: each CRC-valid row naming the identity is one agreeing
//!   vote at the row's own capture time ([`IdentityTally`]), and until the scheme's bar of votes
//!   has fallen **within its capture-time window** (10 in 5 s for `rds-pi`: a rate, so a
//!   long-running pipeline on a chance lock cannot accumulate its way there) the row is written
//!   **without** its identity — no sighting, so no emitter is created, keyed or confirmed by it —
//!   and marked `identity_provisional: true` with `identity_scheme`, `identity_value`,
//!   `identity_votes`, `identity_votes_needed`, `identity_votes_in_window` and
//!   `identity_votes_window_s` in its metadata, linked to the pipeline's target
//!   emitter when it has one. It is the same bar `hk-demod`'s always-on RDS decoder applies, so an
//!   RDS PI becomes an identity on the same evidence whichever decoder heard it. One frame is at
//!   least one RDS group, so counting frames never credits more groups than were received. The
//!   bar lives in the scheme, not the recipe, so a user-saved copy of a recipe cannot lower it.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use hk_blocks::{Output, PortVec};
use hk_model::{
    ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, EmitterId, Repository, Timestamp,
    VoteWindow,
};
use hk_plugins::{Ingest, MetadataPolicy, output_metadata_policy};
use hk_recipe::{DecodeMapping, OutputSpec, Recipe};
use hk_stream::inspector::{FitStatus, LayerNode, LayerTree};
use hk_stream::{Publisher, PublisherConfig, PublisherHandle, StreamHeader, StreamKind, policy};
use serde_json::{Map, Value};

use crate::chains::plugin::classify_decoder_emitters;
use crate::recipes::runtime::PipelineStats;
use crate::recipes::taps::{FrameCtx, StreamCtx};
use crate::run::Shared;
use crate::stats::{add, inc};

/// Frames a `messages` output queues for its writer before dropping (and counting) more.
pub const MESSAGE_QUEUE: usize = 1024;
/// `message_schema` of a recipe decode stream (the plugin decode schema), also the fallback
/// frame model when a restricted policy does not allowlist the mapping's.
pub const DECODE_MESSAGE_SCHEMA: &str = "hackriff.decode/1";

/// How one `messages` output maps a frame's layer tree to a Decode row.
#[derive(Clone, Debug)]
pub struct RowSpec {
    mapping: DecodeMapping,
    decoder_id: String,
    decoder_version: String,
    class: ContentClass,
    metadata: Vec<(String, String)>,
    content: Vec<(String, String)>,
}

impl RowSpec {
    /// The row spec of `mapping` in `recipe`, under the pipeline's effective `class`. Rows name
    /// decoder `recipe:<id>` at the recipe's version.
    pub fn new(recipe: &Recipe, mapping: DecodeMapping, class: ContentClass) -> Self {
        Self {
            metadata: keyed(&mapping.metadata),
            content: keyed(&mapping.content),
            decoder_id: format!("recipe:{}", recipe.id),
            decoder_version: recipe.version.to_string(),
            class,
            mapping,
        }
    }

    /// The Decode row of a CRC-valid frame parsed into `tree` at `t`, or `None` when the frame
    /// lacks a `require`d field (by default: when no mapped field is present). The row is not yet
    /// sanitised: the writer applies the output policy and the repository gate.
    pub fn decode(&self, tree: &LayerTree, t: Timestamp) -> Option<Decode> {
        let m = &self.mapping;
        if !m.require.iter().all(|p| present(tree, p).is_some()) {
            return None;
        }
        let identity = m.identity.as_ref().and_then(|i| {
            let scheme = i.identity_scheme()?;
            let n = present(tree, &i.field)?;
            let hex = i.format == hk_recipe::Display::Hex;
            let value = match n.value.as_ref()? {
                Value::Number(x) => scheme.canonical_uint(x.as_u64()?, n.bits[1], hex),
                Value::String(s) => scheme.canonical_uint(s.parse().ok()?, n.bits[1], hex),
                _ => return None,
            };
            Some(DecodedIdentity { scheme, value })
        });
        let fill = |keys: &[(String, String)]| {
            let mut out = Map::new();
            for (path, key) in keys {
                if let Some(v) = present(tree, path).and_then(|n| n.value.clone()) {
                    out.insert(key.clone(), v);
                }
            }
            out
        };
        let metadata = fill(&self.metadata);
        let content = fill(&self.content);
        if m.require.is_empty() && identity.is_none() && metadata.is_empty() && content.is_empty() {
            return None;
        }
        Some(Decode {
            id: DecodeId::new(),
            demodulation_ref: None,
            recording_ref: None,
            decoder_id: self.decoder_id.clone(),
            decoder_version: self.decoder_version.clone(),
            frame_model: m.frame_model.clone(),
            metadata: Value::Object(metadata),
            content: (!content.is_empty()).then_some(Value::Object(content)),
            crc_status: CrcStatus::Valid,
            identity,
            content_class: self.class,
            t,
            provenance: None,
        })
    }
}

/// A present, decoded field (not a layer, not a failed field).
fn present<'a>(tree: &'a LayerTree, path: &str) -> Option<&'a LayerNode> {
    tree.node(path).filter(|n| !n.error && n.value.is_some())
}

/// `(path, key)`: the key is the path's last segment unless another path in `paths` shares it.
fn keyed(paths: &[String]) -> Vec<(String, String)> {
    let last = |p: &str| p.rsplit('.').next().unwrap_or(p).to_owned();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for p in paths {
        *seen.entry(last(p)).or_default() += 1;
    }
    paths
        .iter()
        .map(|p| {
            let k = last(p);
            let key = if seen[&k] > 1 { p.clone() } else { k };
            (p.clone(), key)
        })
        .collect()
}

/// Most distinct identities one writer tallies; a frame naming another once full stays
/// provisional (fails closed; a noise-driven spray of values cannot grow the map).
const MAX_TALLIED: usize = 256;

/// Agreeing CRC-valid frames per identity, for schemes with a vote bar (T-962; module docs),
/// each held as an [`hk_model::VoteWindow`] over the rows' own capture times.
#[derive(Debug, Default)]
pub struct IdentityTally {
    votes: BTreeMap<(hk_model::IdentityScheme, String), VoteWindow>,
}

impl IdentityTally {
    /// Counts `d`'s identity as one more agreeing vote at the row's capture time `d.t` and, for a
    /// scheme with a vote bar, records the vote in `d`'s metadata; until the scheme's bar of votes
    /// has fallen within its capture-time window ([`hk_model::IdentityScheme::commit_window_ns`])
    /// it moves the identity off the row into that metadata. Returns whether the row was made
    /// provisional. Rows without an identity, and schemes a single frame suffices for, pass
    /// untouched.
    pub fn gate(&mut self, d: &mut Decode) -> bool {
        let Some(id) = d.identity.as_ref() else {
            return false;
        };
        let needed = id.scheme.commit_votes();
        if needed <= 1 {
            return false;
        }
        let window_ns = id.scheme.commit_window_ns();
        let t = d.t.as_unix_nanos();
        let key = (id.scheme.clone(), id.value.clone());
        let full = self.votes.len() >= MAX_TALLIED;
        let (committed, votes, in_window) = match self.votes.get_mut(&key) {
            Some(w) => (w.vote(t, needed, window_ns), w.votes(), w.window_votes()),
            None => {
                let mut w = VoteWindow::default();
                let c = w.vote(t, needed, window_ns);
                let r = (c, w.votes(), w.window_votes());
                if !full {
                    self.votes.insert(key, w);
                }
                r
            }
        };
        let provisional = !committed;
        if !d.metadata.is_object() {
            d.metadata = Value::Object(Map::new());
        }
        let Value::Object(m) = &mut d.metadata else {
            unreachable!("metadata was just made an object");
        };
        m.insert("identity_provisional".into(), Value::Bool(provisional));
        m.insert("identity_votes".into(), votes.into());
        m.insert("identity_votes_needed".into(), needed.into());
        m.insert("identity_votes_in_window".into(), in_window.into());
        m.insert(
            "identity_votes_window_s".into(),
            (window_ns as f64 / 1e9).into(),
        );
        if provisional && let Some(id) = d.identity.take() {
            m.insert("identity_scheme".into(), id.scheme.as_string().into());
            m.insert("identity_value".into(), id.value.into());
        }
        provisional
    }
}

/// One frame queued for a writer.
pub struct QueuedFrame {
    /// Frame time.
    pub t: Timestamp,
    /// Channel centre of the frame, Hz.
    pub channel_hz: f64,
    /// The frame's layer tree.
    pub layers: Arc<LayerTree>,
}

/// The pipeline-thread end of a `messages` output. Dropping it closes the queue and waits for
/// the writer to store what was queued (always off the pipeline thread's chunk loop: at an edit
/// on the calling thread, or when the pipeline has ended).
pub struct MessagesSink {
    tx: Option<SyncSender<QueuedFrame>>,
    writer: Option<JoinHandle<()>>,
    stats: Arc<PipelineStats>,
}

impl MessagesSink {
    /// A sink whose accepted frames go to the returned receiver instead of a writer thread
    /// (`capacity` frames; tests use it to check the pipeline-thread side on its own).
    pub fn with_queue(capacity: usize, stats: Arc<PipelineStats>) -> (Self, Receiver<QueuedFrame>) {
        let (tx, rx) = mpsc::sync_channel(capacity);
        (
            Self {
                tx: Some(tx),
                writer: None,
                stats,
            },
            rx,
        )
    }

    /// Starts the writer of output `spec` and returns the sink plus the decode stream it
    /// republishes on (none under a class that forbids content without an `output_policy`
    /// allowlist, as for plugins).
    pub(crate) fn spawn(
        shared: &Arc<Shared>,
        ctx: &StreamCtx,
        recipe: &Recipe,
        spec: &OutputSpec,
        stats: Arc<PipelineStats>,
    ) -> Result<(Self, Option<(StreamHeader, PublisherHandle)>), String> {
        let mapping = spec
            .decode
            .clone()
            .ok_or("a messages output needs a decode mapping")?;
        let policy = policy_or_warn(recipe, &spec.id);
        let mut header = StreamHeader::new(
            format!("decodes/{}/{}", ctx.pipeline_id, spec.id),
            StreamKind::Messages,
            ctx.class,
            format!("recipe:{}@{}", recipe.id, recipe.version),
        );
        header.message_schema = Some(DECODE_MESSAGE_SCHEMA.into());
        header.max_frame_len = 64 * 1024;
        let config = PublisherConfig {
            queue_bytes: 4 << 20,
            ..PublisherConfig::default()
        };
        let publisher = if ctx.class.permits_content() {
            Publisher::new(header.clone(), config).ok()
        } else {
            policy
                .clone()
                .and_then(|p| Publisher::with_metadata_policy(header.clone(), config, p).ok())
        };
        let stream = publisher.as_ref().map(|p| (header.clone(), p.handle()));
        let repo = Repository::open(&shared.db_path).map_err(|e| format!("repository: {e}"))?;
        let evidence = mapping.service.clone().unwrap_or_else(|| recipe.id.clone());
        let (classify, errors) = (Arc::clone(shared), Arc::clone(shared));
        let writer = Writer {
            stats: Arc::clone(&stats),
            ingest: match publisher {
                Some(p) => Ingest::with_republish(repo, p),
                None => Ingest::new(repo),
            },
            row: RowSpec::new(recipe, mapping, ctx.class),
            policy,
            emitter: ctx.emitter_id,
            bandwidth_hz: ctx.bandwidth_hz,
            max_batch: MAX_BATCH,
            tally: IdentityTally::default(),
            on_emitters: Box::new(move |new, t| {
                classify_decoder_emitters(&classify, &evidence, None, new, t);
            }),
            on_error: Box::new(move |n| add(&errors.counters.chains.errors, n)),
        };
        Ok((Self::start(writer, stats)?, stream))
    }

    /// A writer for output `output_id` of `recipe` storing into the repository at `db_path` under
    /// `class`, without a run: no republish, no emitter context, and the family step replaced by
    /// `on_emitters` (the emitters first seen per batch). Transactions hold at most `max_batch`
    /// rows. For tests and benchmarks of the writer side; a pipeline uses its own sinks.
    #[doc(hidden)]
    pub fn spawn_standalone(
        db_path: &Path,
        recipe: &Recipe,
        output_id: &str,
        class: ContentClass,
        max_batch: usize,
        stats: Arc<PipelineStats>,
        on_emitters: impl FnMut(&[EmitterId], Timestamp) + Send + 'static,
    ) -> Result<Self, String> {
        Self::spawn_standalone_targeting(
            db_path,
            recipe,
            output_id,
            class,
            None,
            max_batch,
            stats,
            on_emitters,
        )
    }

    /// [`Self::spawn_standalone`] with a target emitter (the pipeline's `{emitter_id}`), as a
    /// recipe started on an inventory entry has.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_standalone_targeting(
        db_path: &Path,
        recipe: &Recipe,
        output_id: &str,
        class: ContentClass,
        emitter: Option<EmitterId>,
        max_batch: usize,
        stats: Arc<PipelineStats>,
        on_emitters: impl FnMut(&[EmitterId], Timestamp) + Send + 'static,
    ) -> Result<Self, String> {
        let spec = recipe
            .outputs
            .iter()
            .find(|o| o.id == output_id)
            .ok_or("no such output")?;
        let mapping = spec
            .decode
            .clone()
            .ok_or("a messages output needs a decode mapping")?;
        let repo = Repository::open(db_path).map_err(|e| format!("repository: {e}"))?;
        let writer = Writer {
            stats: Arc::clone(&stats),
            ingest: Ingest::new(repo),
            row: RowSpec::new(recipe, mapping, class),
            policy: policy_or_warn(recipe, output_id),
            emitter,
            bandwidth_hz: 0.0,
            max_batch: max_batch.max(1),
            tally: IdentityTally::default(),
            on_emitters: Box::new(on_emitters),
            on_error: Box::new(|_| {}),
        };
        Self::start(writer, stats)
    }

    fn start(writer: Writer, stats: Arc<PipelineStats>) -> Result<Self, String> {
        let (tx, rx) = mpsc::sync_channel(MESSAGE_QUEUE);
        let join = thread::Builder::new()
            .name("hk-recipe-decodes".into())
            .spawn(move || writer.run(rx))
            .map_err(|e| format!("spawn: {e}"))?;
        Ok(Self {
            tx: Some(tx),
            writer: Some(join),
            stats,
        })
    }

    /// Queues the output's CRC-valid, fitted frames for the writer (pipeline thread: no
    /// allocation, never blocks); returns frames queued.
    pub fn publish(
        &mut self,
        out: &Output,
        ctx: &FrameCtx<'_>,
        t_of: &dyn Fn(f64) -> Timestamp,
    ) -> u64 {
        let (PortVec::Frames(buf), Some(tx)) = (&out.data, &self.tx) else {
            return 0;
        };
        let mut queued = 0;
        for f in buf.iter() {
            if f.info.check != CrcStatus::Valid {
                continue;
            }
            let Some(layers) = f
                .info
                .layers
                .as_ref()
                .filter(|l| matches!(l.fit, FitStatus::Ok | FitStatus::Partial))
            else {
                continue;
            };
            let job = QueuedFrame {
                t: t_of(f.info.source_index as f64),
                channel_hz: ctx
                    .channels_hz
                    .get(usize::from(f.info.channel))
                    .copied()
                    .unwrap_or(ctx.channel_hz),
                layers: Arc::clone(layers),
            };
            if tx.try_send(job).is_ok() {
                queued += 1;
            } else {
                inc(&self.stats.decodes_dropped);
            }
        }
        queued
    }
}

impl Drop for MessagesSink {
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(j) = self.writer.take() {
            let _ = j.join();
        }
    }
}

/// Rows a writer stores per transaction when its queue has backed up (T-112).
pub const MAX_BATCH: usize = 128;

/// The recipe's `output_policy` as the manifest rules parse it: `Err` (with the reason) when
/// malformed, `Ok(None)` when it declares no allowlist.
pub(crate) fn recipe_output_policy(recipe: &Recipe) -> Result<Option<MetadataPolicy>, String> {
    let v = serde_json::to_value(&recipe.output_policy).map_err(|e| e.to_string())?;
    output_metadata_policy(&v).map_err(|e| e.to_string())
}

/// [`recipe_output_policy`], failing closed: a malformed policy is no policy (restricted rows
/// reduced to the empty allowlist, nothing republished) and is logged. Staging reports it as a
/// pipeline warning ([`crate::recipes::graph::stage`]).
fn policy_or_warn(recipe: &Recipe, output_id: &str) -> Option<MetadataPolicy> {
    recipe_output_policy(recipe).unwrap_or_else(|e| {
        eprintln!(
            "hk-pipeline: warning: recipe {}@{} output {output_id}: malformed output_policy ({e}); \
             restricted decodes keep no metadata and are not republished",
            recipe.id, recipe.version
        );
        None
    })
}

/// Emitters first seen in a batch, and the batch's last frame time.
type OnEmitters = Box<dyn FnMut(&[EmitterId], Timestamp) + Send>;

/// The off-thread end: sanitise, store, attach, classify, republish.
struct Writer {
    stats: Arc<PipelineStats>,
    ingest: Ingest,
    row: RowSpec,
    policy: Option<MetadataPolicy>,
    emitter: Option<EmitterId>,
    bandwidth_hz: f64,
    max_batch: usize,
    /// Agreeing votes per weak identity (T-962).
    tally: IdentityTally,
    /// The family step for new emitters.
    on_emitters: OnEmitters,
    /// Counts rows that could not be stored.
    on_error: Box<dyn FnMut(u64) + Send>,
}

impl Writer {
    fn run(mut self, rx: Receiver<QueuedFrame>) {
        let mut batch = Vec::with_capacity(self.max_batch);
        while let Ok(first) = rx.recv() {
            // Whatever queued up behind the first frame (at most `max_batch`) goes in one
            // transaction: one commit per batch instead of two per row when the queue backs up.
            batch.push(first);
            while batch.len() < self.max_batch {
                match rx.try_recv() {
                    Ok(job) => batch.push(job),
                    Err(_) => break,
                }
            }
            self.store_batch(&mut batch);
        }
        if let Some(p) = self.ingest.take_publisher() {
            p.finish();
        }
    }

    /// Stores `batch` (drained) in one transaction, then runs the family step for the emitters it
    /// saw first (after the commit, so the step's own connection sees them).
    fn store_batch(&mut self, batch: &mut Vec<QueuedFrame>) {
        let Some(last_t) = batch.last().map(|j| j.t) else {
            return;
        };
        let (row, policy) = (&self.row, self.policy.as_ref());
        let (emitter, bandwidth_hz) = (self.emitter, self.bandwidth_hz);
        let tally = &mut self.tally;
        let mut attempted = 0u64;
        let result = self.ingest.batch(|ingest| {
            store_rows(
                ingest,
                row,
                policy,
                (emitter, bandwidth_hz),
                tally,
                batch,
                &mut attempted,
            )
        });
        let (ok, failed) = match result {
            Ok(counts) => counts,
            // The batch could not begin (the closure never ran, nothing was drained): store the
            // rows one transaction each, as before T-112, so none is lost uncounted.
            Err(_) if !batch.is_empty() => store_rows(
                &mut self.ingest,
                row,
                policy,
                (emitter, bandwidth_hz),
                tally,
                batch,
                &mut attempted,
            ),
            // The commit failed: every attempted row was rolled back, and so were the emitters
            // they created (their ids must not reach the family step).
            Err(_) => {
                drop(self.ingest.take_new_emitters());
                (0, attempted)
            }
        };
        batch.clear();
        add(&self.stats.decodes, ok);
        (self.on_error)(failed);
        let new = self.ingest.take_new_emitters();
        if !new.is_empty() {
            (self.on_emitters)(&new, last_t);
        }
    }
}

/// Stores the rows of `batch` (drained) through `ingest`, each with its own sanitising, shape
/// check and repository content gate exactly as before T-112. Returns `(stored, failed)` and
/// counts rows that mapped to a Decode in `attempted`.
fn store_rows(
    ingest: &mut Ingest,
    row: &RowSpec,
    policy: Option<&MetadataPolicy>,
    (emitter, bandwidth_hz): (Option<EmitterId>, f64),
    tally: &mut IdentityTally,
    batch: &mut Vec<QueuedFrame>,
    attempted: &mut u64,
) -> (u64, u64) {
    let (mut ok, mut failed) = (0u64, 0u64);
    for job in batch.drain(..) {
        let Some(mut d) = row.decode(&job.layers, job.t) else {
            continue;
        };
        drop(job.layers);
        *attempted += 1;
        // T-962: below its scheme's vote bar the identity comes off the row before anything
        // downstream (sanitising, the sighting, the republish) can see it.
        let provisional = tally.gate(&mut d);
        // The same sanitising as `hk_plugins::output::parse_line`: a no-op under a class that
        // permits content, the output policy's allowlist otherwise.
        policy::sanitize_decode(policy, DECODE_MESSAGE_SCHEMA, &mut d);
        let stored = match emitter {
            // A provisional row is a record, not a claim: linked to the target so the emitter's
            // decode route can show it, with no identity sighting.
            Some(target) if provisional => ingest.store_decode_linked(d, target, None),
            _ => ingest.store_decode(d, emitter, None, Some(job.channel_hz), Some(bandwidth_hz)),
        };
        match stored {
            Ok(_) => ok += 1,
            Err(_) => failed += 1,
        }
    }
    (ok, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_stream::inspector::NodeType;
    use serde_json::json;

    fn node(id: u32, path: &str, bits: u32, value: Option<Value>) -> LayerNode {
        LayerNode {
            id,
            parent: None,
            name: path.rsplit('.').next().unwrap().into(),
            path: path.into(),
            ty: NodeType::Uint,
            bits: [0, bits],
            bytes: [0, bits.div_ceil(8)],
            value,
            text: None,
            label: None,
            error: false,
        }
    }

    fn tree(nodes: Vec<LayerNode>) -> LayerTree {
        LayerTree {
            nodes,
            byte_index: Vec::new(),
            fit: FitStatus::Ok,
            errors: Vec::new(),
        }
    }

    fn recipe(outputs: Value, class: &str) -> Recipe {
        crate::recipes::runtime::parse_recipe(json!({
            "schema": "hackriff.recipe", "schema_version": 2, "id": "adsb", "version": 3,
            "name": "A", "input": {"port": "frames"},
            "nodes": [{"id": "msg", "block": "identity"}],
            "outputs": outputs,
            "output_policy": {"content_class": class,
                              "metadata_keys": {"function": {"type": "enum",
                                                             "values": ["0", "1", "2", "3"]}}}
        }))
        .unwrap()
    }

    fn spec(r: &Recipe) -> RowSpec {
        let m = r.outputs[0].decode.clone().unwrap();
        RowSpec::new(r, m, r.output_policy.content_class)
    }

    #[test]
    fn fields_map_to_a_decode_row_with_a_canonical_identity() {
        let r = recipe(
            json!([{"id": "aircraft", "kind": "messages", "from": "msg", "decode": {
                "frame_model": "adsb-es",
                "identity": {"scheme": "adsb-icao", "field": "icao", "format": "hex"},
                "metadata": ["df", "me.tc", "me.velocity.tc"],
                "content": ["me.text"],
                "require": ["icao"], "service": "adsb"}}]),
            "unrestricted",
        );
        let t = Timestamp::from_unix_nanos(5);
        let full = tree(vec![
            node(0, "df", 5, Some(json!(17))),
            node(1, "icao", 24, Some(json!(0x00abcd))),
            node(2, "me.tc", 5, Some(json!(19))),
            node(3, "me.velocity.tc", 5, Some(json!(1))),
            node(4, "me.text", 0, Some(json!("HELLO"))),
        ]);
        let d = spec(&r).decode(&full, t).unwrap();
        assert_eq!(
            (d.decoder_id.as_str(), d.decoder_version.as_str()),
            ("recipe:adsb", "3")
        );
        assert_eq!(d.frame_model, "adsb-es");
        assert_eq!(d.crc_status, CrcStatus::Valid);
        assert_eq!(d.t, t);
        let id = d.identity.unwrap();
        assert_eq!(
            (id.scheme, id.value.as_str()),
            (hk_model::IdentityScheme::AdsbIcao, "00abcd")
        );
        // A shared last segment keeps the whole path for both.
        assert_eq!(
            d.metadata,
            json!({"df": 17, "me.tc": 19, "me.velocity.tc": 1})
        );
        assert_eq!(d.content, Some(json!({"text": "HELLO"})));

        // A required field missing (or failed): no row.
        let mut failed = full.clone();
        failed.nodes[1].error = true;
        assert!(spec(&r).decode(&failed, t).is_none());
        let no_icao = tree(vec![node(0, "df", 5, Some(json!(4)))]);
        assert!(spec(&r).decode(&no_icao, t).is_none());
    }

    #[test]
    fn without_require_any_mapped_field_emits_and_none_does_not() {
        let r = recipe(
            json!([{"id": "station", "kind": "messages", "from": "msg", "decode": {
                "frame_model": "rds-ps",
                "identity": {"scheme": "rds-pi", "field": "ps.key", "format": "hex"},
                "content": ["ps.text"]}}]),
            "unrestricted",
        );
        let t = Timestamp::from_unix_nanos(1);
        let ps = tree(vec![
            node(0, "ps.key", 0, Some(json!(0xc0de))),
            node(1, "ps.text", 64, Some(json!("HACKRIFF"))),
        ]);
        let d = spec(&r).decode(&ps, t).unwrap();
        assert_eq!(d.identity.unwrap().value, "C0DE");
        assert_eq!(d.metadata, json!({}));
        assert_eq!(d.content, Some(json!({"text": "HACKRIFF"})));
        let unrelated = tree(vec![node(0, "other", 8, Some(json!(1)))]);
        assert!(spec(&r).decode(&unrelated, t).is_none());
    }

    #[test]
    fn an_unknown_token_scheme_is_other_and_decimal_by_default() {
        let r = recipe(
            json!([{"id": "page", "kind": "messages", "from": "msg", "decode": {
                "frame_model": "pocsag-page",
                "identity": {"scheme": "pocsag-ric", "field": "ric"},
                "metadata": ["function"]}}]),
            "restricted-paging",
        );
        let row = spec(&r);
        let d = row
            .decode(
                &tree(vec![
                    node(0, "ric", 21, Some(json!(1234567))),
                    node(1, "function", 2, Some(json!(3))),
                ]),
                Timestamp::from_unix_nanos(1),
            )
            .unwrap();
        let id = d.identity.unwrap();
        assert_eq!(
            id.scheme,
            hk_model::IdentityScheme::Other("pocsag-ric".into())
        );
        assert_eq!(id.value, "1234567");
        assert_eq!(d.content_class, ContentClass::RestrictedPaging);
    }
}
