//! Burst taps (T-060, workflow steps 6-7): the bits and soft symbols of the bursts the FSK chains
//! demodulate, streamed to external programs. [`BurstTapOpener`] is a [`StreamOpener`]
//! (`open/bits`, `open/symbols` on TCP; `/ws/open/bits`, `/ws/open/symbols` on WebSocket).
//!
//! # Flow
//! - A request (every burst, an emitter, a detection or a selected extent) registers a **tap**
//!   on the run's [`BurstHub`]: one [`Publisher`] with one consumer. Nothing reads the ring for a
//!   tap; it costs nothing until bursts arrive.
//! - FSK chains ([`super::fsk`]) offer bursts to the hub **while they run** once `min_bursts`
//!   are demodulated and a tap exists (framing inferred over the bursts so far, at most every
//!   [`STREAM_INFER_INTERVAL`]), and the rest when they detach (final framing, emitter id).
//! - Per burst, each matching tap publishes a status record ([`BurstStatus`]) and a data record
//!   (`ru8` bits or `rf32_le` soft symbols, in the framing model's polarity). Publishing never
//!   blocks: the per-consumer queue drops with markers (§7).
//! - Dropping the session guard (client gone) removes the tap; the end of the run closes every
//!   tap, which finishes its stream.
//!
//! # Classes (existing FSK rules, unchanged)
//! A burst's class is the class its Decode rows are stored under
//! ([`hk_demod::fsk::record::effective_content_class`]: fail closed unless a user
//! classification rule vouches for the emitter). A tap's header class is decided at open:
//! [`classify_emitter`] on the requested extent (fail closed when nothing vouches), or for every
//! burst the source class, lifted to `unrestricted` when classification rules exist for a
//! non-restricted source. Content goes out only when both the burst's and the header's class
//! permit it: a withheld burst sends its status record with `content_withheld: true`; under a
//! header class that forbids content the egress gate sends header-only `GATED` records.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_demod::fsk::FskBurst;
use hk_estimate::framing::{BitOrder, FramingResult};
use hk_model::{BitstreamPayload, ContentClass, EmitterId, Framing, Timestamp};
use hk_stream::bursts::{BITS_DATATYPE, BurstStatus, BurstTarget, SYMBOLS_DATATYPE};
use hk_stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig, RecordFlags,
    StreamError, StreamHeader, StreamKind, StreamOpener,
};
use serde_json::{Value, json};

use super::budget::{ChainKind, Slot, mcores};
use super::listen::{ListenConfig, SegmentFn};
use crate::class::{classify_emitter, is_restricted};
use crate::config::ListenSettings;
use crate::stats::{ChainStatGuard, Counters, inc, set};

/// Default most taps open at once (`ListenSettings::max_taps`); further requests are refused
/// (503). Admission is the run's on-demand chain budget (T-071, [`super::budget`]).
pub const MAX_TAPS: usize = 8;
/// Shortest interval between framing inferences for live tap output.
pub const STREAM_INFER_INTERVAL: Duration = Duration::from_millis(500);
/// Largest record on a tap stream, bytes (64 000 symbols as `f32`).
const TAP_MAX_FRAME_LEN: u32 = 256 * 1024;

/// What a tap streams.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapKind {
    /// Hard bits (`ru8`).
    Bits,
    /// Soft symbols (`rf32_le`).
    Symbols,
}

impl TapKind {
    fn name(self) -> &'static str {
        match self {
            Self::Bits => "bits",
            Self::Symbols => "symbols",
        }
    }
}

struct Tap {
    id: u64,
    kind: TapKind,
    extent: Option<(f64, f64)>,
    emitter: Option<EmitterId>,
    class: ContentClass,
    publisher: Mutex<Publisher>,
    bursts: AtomicU64,
    /// The tap's own counters (T-071).
    stat: ChainStatGuard,
}

/// A demodulated burst ready to publish.
struct Prepared {
    status: BurstStatus,
    bits: Vec<u8>,
    soft: Vec<u8>,
    t: Timestamp,
    sample_index: u64,
    extent: (f64, f64),
}

/// The run's taps (shared by every segment).
#[derive(Default)]
pub struct BurstHub {
    taps: Mutex<Vec<Arc<Tap>>>,
    count: AtomicUsize,
    next: AtomicU64,
    closed: AtomicBool,
}

impl BurstHub {
    /// Whether any tap is open (the chains' fast path).
    pub fn has_taps(&self) -> bool {
        self.count.load(Ordering::Relaxed) > 0
    }

    /// Open taps.
    pub fn taps(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Tap>>> {
        self.taps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn add(&self, tap: Arc<Tap>) -> bool {
        let mut taps = self.lock();
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        taps.push(tap);
        self.count.store(taps.len(), Ordering::Relaxed);
        true
    }

    fn remove(&self, id: u64) -> bool {
        let mut taps = self.lock();
        let before = taps.len();
        taps.retain(|t| t.id != id);
        self.count.store(taps.len(), Ordering::Relaxed);
        taps.len() < before
    }

    /// Ends every tap (their streams finish) and refuses new ones: the run is over.
    pub(crate) fn close(&self) {
        let taps: Vec<Arc<Tap>> = {
            let mut taps = self.lock();
            self.closed.store(true, Ordering::SeqCst);
            self.count.store(0, Ordering::Relaxed);
            taps.drain(..).collect()
        };
        drop(taps);
    }

    /// Offers `bursts[from..]` (framed by `result`, stored under `class`) to every matching tap.
    pub(crate) fn offer(
        &self,
        counters: &Counters,
        bursts: &[FskBurst],
        from: usize,
        result: &FramingResult,
        class: ContentClass,
        emitter: Option<EmitterId>,
    ) {
        let taps: Vec<Arc<Tap>> = self.lock().clone();
        if taps.is_empty() {
            return;
        }
        let c = &counters.taps;
        for (i, burst) in bursts.iter().enumerate().skip(from) {
            let Some(p) = prepare(burst, i, result) else {
                continue;
            };
            inc(&c.bursts);
            for tap in taps
                .iter()
                .filter(|t| t.matches(p.extent.0, p.extent.1, emitter))
            {
                let t0 = std::time::Instant::now();
                tap.publish(counters, &p, class, emitter);
                tap.stat.latency(t0.elapsed().as_micros() as u64);
            }
        }
    }
}

impl Tap {
    fn matches(&self, lo: f64, hi: f64, emitter: Option<EmitterId>) -> bool {
        if self.emitter.is_some() && self.emitter == emitter {
            return true;
        }
        // The burst's centre must lie in the requested extent (padded for estimate spread), so
        // concurrent taps on adjacent channels do not see each other's bursts.
        match self.extent {
            None => true,
            Some((a, b)) => {
                let pad = (0.25 * (b - a)).max(2e3);
                let center = 0.5 * (lo + hi);
                center >= a - pad && center <= b + pad
            }
        }
    }

    fn publish(
        &self,
        counters: &Counters,
        p: &Prepared,
        class: ContentClass,
        emitter: Option<EmitterId>,
    ) {
        let c = &counters.taps;
        let mut publisher = self
            .publisher
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut status = p.status.clone();
        status.burst = self.bursts.fetch_add(1, Ordering::Relaxed);
        status.emitter_id = emitter;
        // Content needs both classes to permit it; own-key content only on an own-key stream.
        let burst_permits = hk_stream::gate::message_content_permitted(self.class, class);
        status.content_withheld = self.class.permits_content() && !burst_permits;
        match publisher.publish_status(p.t, p.sample_index, &status.to_value()) {
            Ok(_) => inc(&c.status_records),
            Err(_) => inc(&c.errors),
        }
        if status.content_withheld {
            inc(&c.withheld);
            return;
        }
        let payload = match self.kind {
            TapKind::Bits => &p.bits,
            TapKind::Symbols => &p.soft,
        };
        match publisher.publish_binary(BinaryRecord {
            t: p.t,
            sample_index: p.sample_index,
            flags: RecordFlags::BURST_START.with(RecordFlags::BURST_END),
            payload,
        }) {
            Ok(_) => {
                inc(&c.records);
                inc(&self.stat.records);
            }
            Err(StreamError::ContentGated { .. }) => inc(&c.gated),
            Err(_) => inc(&c.errors),
        }
    }
}

/// First index of `needle` in `hay`, trying `hint` first, then scanning from `from`.
fn locate(hay: &[u8], needle: &[u8], hint: Option<usize>, from: usize) -> Option<u64> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    if let Some(h) = hint
        && hay.get(h..h + needle.len()) == Some(needle)
    {
        return Some(h as u64);
    }
    (from.min(hay.len())..=hay.len() - needle.len())
        .find(|&k| &hay[k..k + needle.len()] == needle)
        .map(|k| k as u64)
}

fn prepare(burst: &FskBurst, index: usize, result: &FramingResult) -> Option<Prepared> {
    let sy = burst.symbols.as_ref()?;
    if sy.bits.is_empty() {
        return None;
    }
    let frame = result.frames.get(index);
    let inverted = frame.is_some_and(|f| f.inverted);
    let flip = u8::from(inverted);
    let bits: Vec<u8> = sy.bits.iter().map(|b| (b & 1) ^ flip).collect();
    let mut soft = Vec::with_capacity(4 * sy.soft.len());
    for s in &sy.soft {
        let v = if inverted { -*s } else { *s };
        soft.extend_from_slice(&v.to_le_bytes());
    }
    let sync_bit = frame.and_then(|f| f.sync_bit);
    let sync_bits = result.model.sync.as_ref().map(|s| s.length_bits);
    let payload = result.payload(index, &sy.bits);
    let payload_bit = payload.as_ref().and_then(|pb| {
        locate(
            &bits,
            &pb.bits,
            sync_bit.map(|s| s + sync_bits.unwrap_or(0)),
            sync_bit.unwrap_or(0),
        )
    });
    let rate = if sy.lock.tracked_rate_bd.is_finite() && sy.lock.tracked_rate_bd > 0.0 {
        sy.lock.tracked_rate_bd
    } else {
        sy.rate_bd
    };
    let center = burst.rf_center_hz();
    let bw = burst.request.bandwidth_hz.max(0.0);
    let status = BurstStatus {
        symbols: bits.len() as u64,
        symbol_rate_bd: rate,
        f_center_hz: center,
        bandwidth_hz: bw,
        snr_db: sy.symbol_snr_db,
        framed: sync_bit.is_some(),
        inverted,
        sync_bit: sync_bit.map(|s| s as u64),
        sync_bits: sync_bits.map(|s| s as u64),
        payload_bit,
        payload_bits: payload.as_ref().map(|pb| pb.bits.len() as u64),
        bit_order: payload.as_ref().map(|pb| match pb.bit_order {
            BitOrder::MsbFirst => "msb-first",
            BitOrder::LsbFirst => "lsb-first",
        }),
        crc: frame
            .and_then(|f| f.crc_valid)
            .map(|v| if v { "valid" } else { "invalid" }),
        ..BurstStatus::default()
    };
    Some(Prepared {
        status,
        t: burst.timestamp_of_symbol(0),
        sample_index: sy
            .source_index
            .first()
            .map_or(burst.request.start_index, |i| i.max(0.0) as u64),
        bits,
        soft,
        extent: (center - 0.5 * bw, center + 0.5 * bw),
    })
}

/// Removes its tap when dropped (the consumer went away).
struct TapSession {
    hub: Arc<BurstHub>,
    id: u64,
    counters: Arc<Counters>,
    /// The tap's share of the run's chain budget, released when the consumer goes.
    _slot: Slot,
}

impl Drop for TapSession {
    fn drop(&mut self) {
        if self.hub.remove(self.id) {
            inc(&self.counters.taps.detached);
        }
        set(&self.counters.taps.active, self.hub.taps() as u64);
    }
}

/// Opens bits or symbols taps on a running pipeline (`PipelineHandle::bits_service`,
/// `PipelineHandle::symbols_service`).
pub struct BurstTapOpener {
    kind: TapKind,
    hub: Arc<BurstHub>,
    counters: Arc<Counters>,
    segment: SegmentFn,
    /// The run's on-demand limits (T-071 budget), read at each request.
    settings: Arc<Mutex<ListenSettings>>,
}

impl BurstTapOpener {
    pub(crate) fn new(
        kind: TapKind,
        hub: Arc<BurstHub>,
        counters: Arc<Counters>,
        segment: SegmentFn,
        settings: Arc<Mutex<ListenSettings>>,
    ) -> Self {
        Self {
            kind,
            hub,
            counters,
            segment,
            settings,
        }
    }

    fn open_inner(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let target = BurstTarget::from_request(req)?;
        let shared = (self.segment)().ok_or_else(|| {
            OpenRefusal::new(
                503,
                "replumbing",
                "the run is changing window or has ended; try again",
            )
        })?;
        if self.hub.closed.load(Ordering::SeqCst) {
            return Err(OpenRefusal::new(410, "source-ended", "the run has ended"));
        }
        let not_found = |what: &str| OpenRefusal::new(404, "not-found", format!("no such {what}"));
        let (extent, emitter) = match target {
            BurstTarget::All => (None, None),
            BurstTarget::Range { f_lo_hz, f_hi_hz } => (Some((f_lo_hz, f_hi_hz)), None),
            BurstTarget::Emitter(id) => {
                let e = shared
                    .repo()
                    .emitter(id)
                    .map_err(|_| not_found("emitter"))?;
                let h = 0.5 * e.bandwidth_hz.max(0.0);
                (Some((e.f_center_hz - h, e.f_center_hz + h)), Some(id))
            }
            BurstTarget::Detection(id) => {
                let d = shared
                    .repo()
                    .detection(id)
                    .map_err(|_| not_found("detection"))?;
                let h = 0.5 * d.obw_hz.max(0.0);
                (Some((d.f_center_hz - h, d.f_center_hz + h)), None)
            }
        };
        let rules = &shared.cfg.settings.classify;
        let source = shared.cfg.source_class;
        let class = match extent {
            Some((lo, hi)) => classify_emitter(rules, source, lo, hi)
                .map_or(ContentClass::FAIL_CLOSED, |(class, _)| class),
            None if source.permits_content() || is_restricted(source) => source,
            None if !rules.is_empty() => ContentClass::Unrestricted,
            None => source,
        };
        drop(shared);
        let cfg = ListenConfig::from_settings(
            &self.settings.lock().unwrap_or_else(PoisonError::into_inner),
        );
        let slot = Slot::claim(
            &self.counters,
            &cfg.limits(),
            ChainKind::Tap,
            mcores(cfg.tap_cores),
            0.0,
        )?;
        let id = self.hub.next.fetch_add(1, Ordering::Relaxed);
        let (kind, datatype, payload) = match self.kind {
            TapKind::Bits => (StreamKind::Bits, BITS_DATATYPE, BitstreamPayload::HardBits),
            TapKind::Symbols => (
                StreamKind::Symbols,
                SYMBOLS_DATATYPE,
                BitstreamPayload::SoftSymbols,
            ),
        };
        let mut header = StreamHeader::new(
            format!("{}/bursts/{id}", self.kind.name()),
            kind,
            class,
            "hk-pipeline:burst-tap",
        );
        header.datatype = Some(datatype.into());
        if let Some((lo, hi)) = extent {
            header.center_hz = Some(0.5 * (lo + hi));
            header.bandwidth_hz = Some((hi - lo).max(0.0));
        }
        header.emitter_id = emitter;
        header.framing = Some(Framing {
            payload,
            bits_per_symbol: Some(1),
            symbol_rate_hz: None,
            schema_id: None,
            sync_word_hex: None,
        });
        header.max_frame_len = TAP_MAX_FRAME_LEN;
        let publisher = Publisher::new(
            header.clone(),
            PublisherConfig {
                queue_bytes: 2 * 1024 * 1024,
                disconnect_after_drops: u64::MAX,
                disconnect_after: Duration::from_secs(5),
                max_consumers: 1,
                drain_timeout: Duration::from_secs(2),
            },
        )
        .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
        let handle = publisher.handle();
        let stat = self
            .counters
            .chain_stats
            .register(&format!("{}-tap", self.kind.name()));
        stat.set_stream(&header.stream_id, handle.clone());
        if let Some((lo, hi)) = extent {
            stat.set_channel(0.5 * (lo + hi), hi - lo);
        }
        let tap = Arc::new(Tap {
            id,
            kind: self.kind,
            extent,
            emitter,
            class,
            publisher: Mutex::new(publisher),
            bursts: AtomicU64::new(0),
            stat,
        });
        if !self.hub.add(tap) {
            return Err(OpenRefusal::new(410, "source-ended", "the run has ended"));
        }
        inc(&self.counters.taps.attached);
        set(&self.counters.taps.active, self.hub.taps() as u64);
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(TapSession {
                hub: Arc::clone(&self.hub),
                id,
                counters: Arc::clone(&self.counters),
                _slot: slot,
            }),
        })
    }
}

impl StreamOpener for BurstTapOpener {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        inc(&self.counters.taps.requests);
        let result = self.open_inner(request);
        if let Err(e) = &result {
            inc(&self.counters.taps.refused);
            if crate::debug_enabled() {
                eprintln!("hk-pipeline: {} tap refused: {e}", self.kind.name());
            }
        }
        result
    }

    fn describe(&self) -> Value {
        let (kind, datatype, element) = match self.kind {
            TapKind::Bits => ("bits", BITS_DATATYPE, "one u8 (0 or 1) per bit"),
            TapKind::Symbols => (
                "symbols",
                SYMBOLS_DATATYPE,
                "one f32 LE soft value per symbol (LLR-like, positive = 1)",
            ),
        };
        json!({
            "kind": kind,
            "datatype": datatype,
            "params": ["emitter", "detection", "f_lo", "f_hi"],
            "params_note": "none: every burst the run demodulates",
            "records": format!(
                "per burst: a status record (type 3: burst, symbols, symbol_rate_bd, f_center_hz, \
                 bandwidth_hz, framed, inverted, sync_bit, sync_bits, payload_bit, payload_bits, \
                 bit_order, crc, emitter_id, content_withheld) then a data record (type 1, \
                 BURST_START|BURST_END, {element}) in the framing model's polarity"
            ),
            "source": "bursts demodulated by the fsk-bursts chains",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::locate;

    #[test]
    fn locate_prefers_the_hint_then_scans() {
        let hay = [1, 0, 1, 1, 0, 1, 1, 0];
        assert_eq!(locate(&hay, &[1, 1, 0], Some(5), 0), Some(5));
        assert_eq!(locate(&hay, &[1, 1, 0], None, 0), Some(2));
        assert_eq!(locate(&hay, &[1, 1, 0], Some(0), 3), Some(5));
        assert_eq!(locate(&hay, &[0, 0], None, 0), None);
        assert_eq!(locate(&hay, &[], None, 0), None);
    }
}
