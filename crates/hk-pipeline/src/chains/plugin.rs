//! Plugin chain (ADR-0003, SIGNAL-001): ring → DDC to the plugin's rate → ci8 records →
//! `PluginInstance` (a supervised subprocess; GPL decoders stay behind this boundary) → `Ingest`
//! on its own repository connection (SQLite WAL) → `decodes/<plugin>` messages stream.
//!
//! The DDC is skipped when the channel is centred on the tuned centre at the input rate (the
//! samples are passed through unchanged). The input channel class is the source class, a ceiling
//! on the plugin's output. At detach, `tail_pad_samples` zeros flush block-buffered decoders, then
//! the plugin's input ends (queued records delivered, then EOF) and the chain waits for the plugin
//! to flush and exit (`PluginInstance::finish`, T-103). It is stopped only after `settle_s` without
//! progress (a lossless replay allows at least [`PLUGIN_WAIT_STALL`]), so a slow-starting plugin
//! is never cut off before it has read its input.
//!
//! **Start of input (T-223).** The mirror image: a plugin whose manifest declares
//! `input.ready_signal` is not fed until it has sent its `ready` line, so no record reaches a
//! decoder that is still setting up (the readsb wrapper's Beast connection took 1.6 s under load,
//! and the squitters fed meanwhile decoded without their sample time). A lossless replay waits as
//! long as a lossless push would; a live chain waits at most the manifest's `ready_timeout`, then
//! feeds anyway and counts it (`plugin_ready_timeouts`, `plugin_fed_before_ready`). Both waits end
//! at once on shutdown (T-224), like the backpressure wait below. A live chain reads nothing from
//! the ring while it waits, so up to `ready_timeout` of live samples can lap into `lost_samples`
//! (as they can during the 15 s wait for `Running` that precedes it): the bound is a sample-loss
//! budget, which is why the readsb manifest sets it explicitly (docs/stream-contract.md §9.6).
//!
//! **Every wait here is on two budgets, told apart by observation ([`budget`]).** `ready_timeout`
//! and [`PLUGIN_WAIT_STALL`] are budgets on a *subprocess*, and a freshly linked one is spawned in
//! ~193 µs and then executes nothing for up to ~30 s (T-493) — every gate relinks. So they run
//! from the child's **first byte of output**, the host's only evidence it reached its first
//! instruction (`PluginInstance::first_output_at`, T-540); before that byte the manifest's
//! `startup_timeout` governs, and the log says which budget expired and what never arrived.
//!
//! **Backpressure (lossless replay, T-037b).** Plugin input is drop-not-block, which is right for
//! a live source but lost decodes when an unpaced recording outran the plugin (readsb's 8 MiB
//! queue filled on long recordings). In lossless mode the chain waits for queue room before each
//! record ([`wait_for_room`], `plugin_waits`) while its gate cursor holds capture, so nothing is
//! dropped; live chains still drop and count.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::sigmf::Datatype;
use hk_model::{Repository, Timestamp};
use hk_plugins::{
    Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest, PluginState,
};
use hk_stream::{BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamHeader, StreamKind};
use num_complex::Complex;

use hk_model::{EmitterId, TrackId};

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::family::decoder_service_evidence;
use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::{ChainCounters, add, inc};

/// Queue bytes a record needs beyond its payload (length prefix, record header, a drop marker).
const RECORD_OVERHEAD_BYTES: usize = 128;
/// A lossless wait gives up after this long without any change in queue room — **once the plugin
/// has shown a sign of life**. Before that byte the manifest's `startup_timeout` governs instead
/// (see [`budget`]).
const PLUGIN_WAIT_STALL: Duration = Duration::from_secs(30);

/// The two-budget rule every "has the subprocess got there yet?" wait in this file uses, the same
/// one the host's hang watchdog uses (T-540).
///
/// A freshly linked binary is `posix_spawn`ed in ~193 µs and then executes **nothing** for up to
/// ~30 s while the loader and code-signing path warm (T-493), and every gate relinks. So elapsed
/// wall-clock time is not evidence about a child that has produced no output: until its first
/// byte the longer `startup_timeout` applies, measured from when the wait began; from that byte
/// on, the caller's own budget applies, measured from the later of the byte and the last change
/// the wait was watching. Nothing is made slower once the child has started, so a genuine hang is
/// still caught on the tight budget.
fn budget(
    life: Option<Instant>,
    watched_since: Instant,
    normal: Duration,
    startup: Duration,
) -> (Duration, Instant) {
    match life {
        Some(byte) => (normal, watched_since.max(byte)),
        None => (normal.max(startup), watched_since),
    }
}

/// Lossless replay: waits until the plugin's input queue has room for a `payload`-byte record
/// (capped at the queue's `capacity`). Gives up, and the record is offered anyway, on `stop`,
/// when the plugin has failed, or after [`PLUGIN_WAIT_STALL`] without progress
/// (`plugin_wait_timeouts`) — on the [`budget`] rule, so a plugin that has not reached its first
/// instruction is given the manifest's `startup_timeout` instead.
fn wait_for_room(
    inst: &PluginInstance,
    payload: usize,
    capacity: usize,
    stop: &AtomicBool,
    c: &ChainCounters,
) {
    let need = (payload + RECORD_OVERHEAD_BYTES).min(capacity);
    let startup = inst.limits().startup_timeout;
    let mut waited = false;
    let mut last = None;
    let mut since = Instant::now();
    loop {
        let free = inst.input_free_bytes();
        if free.is_some_and(|f| f >= need) {
            return;
        }
        if stop.load(Ordering::SeqCst) || inst.stats().state == PluginState::Failed {
            return;
        }
        if free != last {
            last = free;
            since = Instant::now();
        } else {
            let (bound, from) = budget(inst.first_output_at(), since, PLUGIN_WAIT_STALL, startup);
            if from.elapsed() >= bound {
                inc(&c.plugin_wait_timeouts);
                return;
            }
        }
        if !waited {
            waited = true;
            inc(&c.plugin_waits);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// How a readiness wait ended. The variants exist so the log can name **what never arrived**
/// rather than only how long the chain waited: a decoder that ran and never signalled and one
/// that never ran at all are different failures with different fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadyWait {
    /// The plugin sent its `ready` line (or is ready without one).
    Ready,
    /// The plugin ran — its first byte arrived — and then did not signal within its bound.
    NeverSignalled,
    /// The plugin produced no output at all within the startup budget: it may never have reached
    /// its first instruction.
    NeverStarted,
    /// The run is stopping, or the plugin failed or stopped. Not a readiness timeout (T-224).
    Ended,
}

/// Waits for a plugin that declares `input.ready_signal` to report itself ready (T-223), bounded
/// by `timeout` **and** by shutdown: like [`wait_for_room`], it gives up at once on `stop`, so a
/// plugin that declares readiness and never signals cannot hold this chain thread for the whole
/// bound past a stop or a detach (T-224).
///
/// The bound follows the [`budget`] rule. A `ready` line is output, so a plugin that has produced
/// **no** output has not got as far as the thing being waited for; charging it `timeout` measures
/// the loader, not the decoder (the T-493 cold link, and the shared cause of this test family).
/// Until its first byte the manifest's `startup_timeout` governs; from that byte on, `timeout`
/// does, so a decoder that runs and then fails to signal is still caught just as fast.
fn wait_ready_or_stop(
    inst: &PluginInstance,
    timeout: Duration,
    startup: Duration,
    stop: &AtomicBool,
) -> ReadyWait {
    let began = Instant::now();
    let mon = inst.monitor();
    loop {
        let s = mon.stats();
        if s.ready {
            return ReadyWait::Ready;
        }
        if stop.load(Ordering::SeqCst)
            || matches!(s.state, PluginState::Failed | PluginState::Stopped)
        {
            return ReadyWait::Ended;
        }
        let life = inst.first_output_at();
        let (bound, from) = budget(life, began, timeout, startup);
        let left = bound.saturating_sub(from.elapsed());
        if left.is_zero() {
            return match life {
                Some(_) => ReadyWait::NeverSignalled,
                None => ReadyWait::NeverStarted,
            };
        }
        std::thread::sleep(left.min(Duration::from_millis(2)));
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    spec_id: &str,
    ddc: Option<(f64, f64)>,
    manifest: &str,
    tail_pad_samples: usize,
    settle_s: f64,
    cursor: GateCursor,
) {
    if let Err(e) = run_inner(
        &shared,
        rx,
        cand,
        ddc,
        manifest,
        tail_pad_samples,
        settle_s,
        cursor,
    ) {
        inc(&shared.counters.chains.attach_errors);
        eprintln!("hk-pipeline: chain {spec_id}: {e:#}");
    }
}

fn to_ci8_bytes(samples: &[Complex<i8>], out: &mut Vec<u8>) {
    out.clear();
    for s in samples {
        out.push(s.re as u8);
        out.push(s.im as u8);
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    shared: &Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    ddc: Option<(f64, f64)>,
    manifest: &str,
    tail_pad_samples: usize,
    settle_s: f64,
    cursor: GateCursor,
) -> anyhow::Result<()> {
    let path = {
        let p = PathBuf::from(manifest);
        if p.is_absolute() {
            p
        } else {
            shared.cfg.manifest_root.join(p)
        }
    };
    let mut m =
        PluginManifest::load(&path).with_context(|| format!("loading {}", path.display()))?;
    if !m.executable.contains('/') {
        if let Some(found) = shared
            .cfg
            .plugin_dirs
            .iter()
            .map(|d| d.join(&m.executable))
            .find(|p| p.is_file())
        {
            m.executable = found.to_string_lossy().into_owned();
        }
    }
    let fs = shared.fs;
    let plugin_id = m.id.clone();
    let queue_bytes = m.limits.input_queue_bytes;
    let ready_signal = m.input.ready_signal;
    let ready_timeout = m.limits.ready_timeout;
    let lossless = shared.gate.enabled();
    let mut cr = ChainReader::new(Arc::clone(shared), cand.first_sample, cursor);
    let first = loop {
        match cr.next() {
            Next::Data(c) => break c,
            Next::Closed => return Ok(()),
            Next::Lost | Next::Idle => {
                if matches!(rx.try_recv(), Ok(ChainMsg::Detach)) {
                    return Ok(());
                }
            }
        }
    };
    let chan_center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    let offset = chan_center - first.provenance.tune.center_hz;
    let (want_rate, bandwidth) = ddc.unwrap_or((fs, fs));
    let identity = (want_rate - fs).abs() < 1.0 && offset.abs() < 1.0;
    let mut ddc_node = if identity {
        None
    } else {
        let mut spec = DdcSpec::new(offset, bandwidth);
        spec.output_rate_hz = Some(want_rate);
        Some(Ddc::new(spec, fs).map_err(|e| anyhow::anyhow!("DDC: {e:?}"))?)
    };
    let out_rate = ddc_node.as_ref().map_or(fs, Ddc::output_rate_hz);

    let class = hk_stream::gate::clamp(shared.cfg.source_class, m.output.content_class);
    let mut header = StreamHeader::new(
        format!("decodes/{}", m.id),
        StreamKind::Messages,
        class,
        format!("hk-plugins:{}@{}", m.id, m.version),
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let config = PublisherConfig {
        queue_bytes: 4 << 20,
        ..PublisherConfig::default()
    };
    // Under a class that forbids content the decode stream republishes only through the
    // manifest's metadata policy (T-016, schema `output.schema_id`). A manifest without one still
    // stores its (sanitised) decodes but offers no stream: before T-037b the missing policy
    // failed the whole chain, so no plugin ran under a restricted or metadata-only class.
    let publisher = if class.permits_content() {
        Some(Publisher::new(header.clone(), config)?)
    } else if let Some(policy) = m.output.metadata_policy.clone() {
        header.message_schema = Some(m.output.schema_id.clone());
        Some(Publisher::with_metadata_policy(
            header.clone(),
            config,
            policy,
        )?)
    } else {
        None
    };
    if let (Some(sink), Some(p)) = (&shared.cfg.stream_sink, &publisher) {
        sink(&header, p.handle());
    }
    let repo = Repository::open(&shared.db_path)?;
    let ingest = Arc::new(Mutex::new(match publisher {
        Some(p) => Ingest::with_republish(repo, p),
        None => Ingest::new(repo),
    }));
    let input = InputStreamDesc {
        datatype: Datatype::Ci8,
        sample_rate_hz: out_rate,
        center_hz: Some(chan_center),
        bandwidth_hz: Some(bandwidth.min(out_rate)),
        content_class: shared.cfg.source_class,
        anchor: first.time,
        emitter_id: None,
        provenance_ref: None,
    };
    let mut inst = PluginInstance::spawn(m, input, PluginContext::default(), Arc::clone(&ingest))
        .context("spawning the plugin")?;
    // Records offered before the subprocess reads its input are dropped (drop-not-block), so the
    // first push waits for it; meanwhile the chain's gate cursor (lossless replay) or the ring's
    // history (live) holds the samples.
    let started = inst
        .monitor()
        .wait_for(Duration::from_secs(15), |s| s.state == PluginState::Running);
    if !started {
        eprintln!("hk-pipeline: plugin did not reach Running within 15 s; feeding anyway");
    }

    let c = &shared.counters.chains;
    // Readiness (T-223): `Running` only means the process is attached. A decoder that declares
    // `input.ready_signal` may still be setting up what it needs to account for input (the readsb
    // wrapper's Beast connection and pre-roll), and records offered before that lose their sample
    // time. A lossless replay waits as long as a lossless push would — its gate cursor holds
    // capture, so nothing is lost. A live chain cannot be held back, so it waits at most the
    // manifest's `ready_timeout` and then feeds anyway, counted and logged.
    if ready_signal {
        let wait = if lossless {
            ready_timeout.max(PLUGIN_WAIT_STALL)
        } else {
            ready_timeout
        };
        // Bounded by the timeout and by shutdown (T-224): a stop or detach must not wait out a
        // plugin that never signals. A stop is not a readiness timeout, so it is not counted.
        let startup = inst.limits().startup_timeout;
        match wait_ready_or_stop(&inst, wait, startup, &shared.stop) {
            ReadyWait::Ready => {}
            // A stop is not a readiness timeout (T-224); a plugin that died before signalling is
            // one, and says which it was.
            ReadyWait::Ended => {
                if !shared.stop.load(Ordering::SeqCst) {
                    inc(&c.plugin_ready_timeouts);
                    eprintln!(
                        "hk-pipeline: plugin {plugin_id} failed or stopped before reporting \
                         ready; feeding anyway"
                    );
                }
            }
            ReadyWait::NeverSignalled => {
                inc(&c.plugin_ready_timeouts);
                eprintln!(
                    "hk-pipeline: plugin {plugin_id} ran but sent no ready line within {wait:?} \
                     of its first output; feeding anyway"
                );
            }
            ReadyWait::NeverStarted => {
                inc(&c.plugin_ready_timeouts);
                eprintln!(
                    "hk-pipeline: plugin {plugin_id} produced no output at all within \
                     {startup:?} (it may never have reached its first instruction); feeding anyway"
                );
            }
        }
    }
    let mut bytes = Vec::new();
    let mut out_index = first.time.sample_index;
    let mut push = |inst: &mut PluginInstance,
                    ddc_node: &mut Option<Ddc>,
                    chunk: &hk_core::ReadChunk,
                    samples: &[Complex<i8>]|
     -> Timestamp {
        match ddc_node {
            None => {
                to_ci8_bytes(samples, &mut bytes);
                out_index = chunk.first_sample();
            }
            Some(d) => match d.process(InputInfo::from(chunk), samples) {
                Ok(block) => {
                    bytes.clear();
                    for z in block.samples {
                        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8 as u8;
                        bytes.push(q(z.re));
                        bytes.push(q(z.im));
                    }
                }
                Err(_) => bytes.clear(),
            },
        }
        let n = (bytes.len() / 2) as u64;
        if n > 0 {
            if lossless {
                wait_for_room(inst, bytes.len(), queue_bytes, &shared.stop, c);
            }
            let _ = inst.push(BinaryRecord {
                t: chunk.time.host_time,
                sample_index: out_index,
                flags: RecordFlags::empty(),
                payload: &bytes,
            });
            add(&c.plugin_samples, n);
            if ddc_node.is_some() {
                out_index += n;
            }
        }
        chunk.time.host_time
    };
    let first_len = first.len;
    let mut last_t = push(&mut inst, &mut ddc_node, &first, &cr.buf[..first_len]);
    cr.release_to(first.end_sample());
    let mut next_index = first.end_sample();
    let mut detached = false;
    loop {
        if !detached && matches!(rx.try_recv(), Ok(ChainMsg::Detach)) {
            detached = true;
        }
        // A detach while the stream runs (coverage lost, manual) ends the chain now; at the end
        // of the stream (ring closed) the chain first feeds what is left in the ring, so a
        // replay shorter than the plugin's start-up is still decoded.
        if detached && !shared.ring.is_closed() {
            break;
        }
        match cr.next() {
            Next::Data(ch) => {
                let samples = cr.buf[..ch.len].to_vec();
                last_t = push(&mut inst, &mut ddc_node, &ch, &samples);
                cr.release_to(ch.end_sample());
                next_index = ch.end_sample();
            }
            Next::Lost | Next::Idle => {}
            Next::Closed => break,
        }
    }
    drop(cr);
    if tail_pad_samples > 0 {
        let zeros = vec![0u8; 2 * 4096];
        let mut left = tail_pad_samples;
        let mut idx = if ddc_node.is_some() {
            out_index
        } else {
            next_index
        };
        while left > 0 {
            let n = left.min(4096);
            if lossless {
                wait_for_room(&inst, 2 * n, queue_bytes, &shared.stop, c);
            }
            let _ = inst.push(BinaryRecord {
                t: last_t,
                sample_index: idx,
                flags: RecordFlags::empty(),
                payload: &zeros[..2 * n],
            });
            idx += n as u64;
            left -= n;
        }
    }
    // End of input (T-103): the plugin gets everything queued, then EOF, and the chain waits for
    // it to flush and exit. The wait is on the plugin's progress, not a wall-clock settle window
    // from the last push, which cut off a plugin still starting up (0 decodes under load). The
    // ring is not held here: the chain's reader was dropped above. A lossless replay allows a
    // stalled plugin as long as a lossless push would; a live chain allows `settle_s`.
    let settle = Duration::from_secs_f64(settle_s.max(0.0));
    let stats = inst.finish(if lossless {
        settle.max(PLUGIN_WAIT_STALL)
    } else {
        settle
    });
    let mut ing = ingest.lock().unwrap_or_else(PoisonError::into_inner);
    add(&c.plugin_decodes, ing.stats().decodes_stored);
    add(
        &c.plugin_fed_before_ready,
        stats.records_offered_before_ready,
    );
    add(
        &c.plugin_dropped,
        stats.records_dropped_full + stats.records_dropped_detached,
    );
    add(&c.plugin_restarts, stats.restarts);
    if let Some(p) = ing.take_publisher() {
        p.finish();
    }
    let emitters = ing.take_new_emitters();
    drop(ing);
    classify_decoder_emitters(shared, &plugin_id, cand.track, &emitters, last_t);
    Ok(())
}

/// The T-039 family step for decoder decodes (T-037b plugin chains; T-111 recipe `messages`
/// outputs). The decoder evidence (a plugin's manifest id, a recipe decode mapping's `service`)
/// maps through [`crate::family`]: `readsb` → `adsb`, `rtl_433` → `ism`. Every emitter the
/// identity decodes resolved to gets that family as a Classification (only when it maps with
/// confidence) **at the decoder arbitration rank** ([`crate::family::decoder_service_evidence`],
/// T-961), then `Inventory::chain_emitter` ranks its explanations and sets its known status, as
/// after the analog and FSK record writers.
///
/// The emitters are the ones the decodes' identities *resolved to* — since T-961 that includes the
/// detection row a recipe's decode was made from, which is the whole point: the CRC-valid decode
/// names, confirms and classifies the entry that was already there instead of a second one beside
/// it. Legal guardrail: this writes a family label, a
/// band-plan reference and a metadata-only annotation, never an identity or content; identities
/// stay gated by their decodes' class.
pub(crate) fn classify_decoder_emitters(
    shared: &Shared,
    plugin_id: &str,
    track: Option<TrackId>,
    emitters: &[EmitterId],
    t: Timestamp,
) {
    if emitters.is_empty() {
        return;
    }
    let c = &shared.counters.chains;
    // T-961: decoder rank, not the classifier's. Every emitter here was reached by a CRC-valid
    // decode's identity sighting, which ADR-0016 §2 ranks above any classifier verdict.
    let evidence = decoder_service_evidence(plugin_id, t);
    let mut repo = shared.repo();
    let mut inv = shared
        .inventory
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    for &e in emitters {
        let live = match repo.live_emitter_id(e) {
            Ok(live) => live,
            Err(err) => {
                crate::stats::storage_error(c, "plugin chain emitter lookup", &err);
                continue;
            }
        };
        if let Some(classification) = &evidence {
            // T-605: named, not a bare count. A classification the database refused is a stage
            // that did not run, and a silent `continue` makes it look like one that did.
            if let Err(err) = repo.append_classification(live, classification) {
                crate::stats::storage_error(c, "plugin chain classification", &err);
                continue;
            }
        }
        // Named, not just counted (T-293/T-319): a chain_emitter failure here silently loses the
        // fold, the T-078 confirmation review and the T-219 overlap resolution, and a bare count
        // trains everyone to ignore it.
        if let Err(err) = inv.chain_emitter(&mut repo, track, live) {
            crate::stats::storage_error(c, "plugin chain emitter", &err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-629: the rule every wait in this file shares. It is stated once here because the three
    /// callers (readiness, queue room, the host's own end-of-input window) are hard to drive to
    /// their expiry in an integration test without a 30 s replay, and getting this wrong is what
    /// produced five separate gate flakes.
    #[test]
    fn a_budget_on_a_subprocess_runs_from_its_first_byte_and_from_the_startup_budget_before_it() {
        let normal = Duration::from_millis(500);
        let startup = Duration::from_secs(60);
        let began = Instant::now();

        // No sign of life: the child may not have reached its first instruction, so elapsed time
        // is no evidence about it and the longer startup budget governs, from when we started
        // watching.
        let (bound, from) = budget(None, began, normal, startup);
        assert_eq!(bound, startup);
        assert_eq!(from, began);

        // The first byte arrived after we started watching: the tight budget applies, timed from
        // the byte, so the loader's share is not charged to the decoder.
        let byte = began + Duration::from_secs(4);
        let (bound, from) = budget(Some(byte), began, normal, startup);
        assert_eq!(bound, normal);
        assert_eq!(from, byte);

        // The child had already spoken before this wait began (queue room, end of input): the
        // budget runs from what we are watching, not from a byte that predates it — otherwise a
        // long-running plugin would be judged to have expired the moment the wait started.
        let earlier = began - Duration::from_secs(10);
        let (bound, from) = budget(Some(earlier), began, normal, startup);
        assert_eq!(bound, normal);
        assert_eq!(from, began);

        // A caller whose own budget is already longer than the startup budget keeps it (the
        // lossless branch's PLUGIN_WAIT_STALL against a short startup_timeout).
        let long = Duration::from_secs(90);
        let (bound, _) = budget(None, began, long, startup);
        assert_eq!(bound, long);
    }
}
