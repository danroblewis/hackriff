//! On-demand channelised IQ opener (T-165, ADR-0013 §4.9 gap 8; stream-contract §12.3):
//! `open/iq?emitter=<id>` or `open/iq?f_lo=<Hz>&f_hi=<Hz>` streams the requested band's raw
//! down-converted samples, `cf32_le`, gated exactly like `bits`/`listen` content.
//!
//! Modelled on [`super::listen`] (the demodulated Listen chain) and `recipes::runtime`'s channel
//! DDC, but simpler: there is no probe, no mode, nothing demodulated — just the channel.
//!
//! # Flow
//! 1. **Target** ([`IqTarget`](hk_stream::iq::IqTarget)): an inventory emitter or an explicit
//!    `f_lo`/`f_hi` band, resolved and bounded to [`hk_stream::iq::MAX_IQ_SPAN_HZ`].
//! 2. **Legal gate**, before any ring read: raw IQ is unambiguously content — more directly than
//!    demodulated audio, since it carries the RF envelope besides — so [`listen_class`] (the
//!    exact rule Listen and burst content already use, T-162's finding applied here) decides
//!    whether it may be served at all. `StreamKind::Iq` is already one of
//!    [`hk_stream::header::StreamKind::payload_is_content`]'s kinds, so the egress gate withholds
//!    the payload on any stream whose class forbids it even if this check were ever bypassed
//!    (belt and braces, as for `bits`/`symbols`/`audio`).
//! 3. **Bounds**: the requested band must lie inside the tuned window; admission shares the run's
//!    on-demand chain budget (T-071) as a [`ChainKind::Tap`] (a raw sample stream, not a
//!    demodulation chain — the same bucket burst taps use), costed like a Listen chain from the
//!    tuned sample rate the DDC's first (input-rate) filter stage must run at, so a wide tuned
//!    window costs more of the budget than a narrow one regardless of how much the channel itself
//!    decimates.
//! 4. **Where the channelisation runs.** On this request's own thread (spawned at open, ended
//!    when the session guard drops), reading the shared ring at its own pace exactly like a
//!    Listen chain: it never touches the capture thread, and a live source skips forward instead
//!    of growing an unbounded backlog (`max_backlog_s`, mirroring Listen/`recipes::runtime`). The
//!    DDC itself only runs while that thread is alive, which only exists while a consumer is
//!    attached: no chain, no DDC, no cost, until `open/iq` is called.
//! 5. **Stream**: the DDC's `cf32_le` output through a drop-not-block [`Publisher`] (§7); a
//!    consumer that stops reading is dropped by the publisher, never the capture thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use hk_core::Discontinuity;
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_stream::iq::{IQ_DATATYPE, IqTarget, MAX_IQ_SPAN_HZ};
use hk_stream::{
    BinaryRecord, OpenRefusal, OpenRequest, OpenedStream, Publisher, PublisherConfig,
    PublisherHandle, RecordFlags, StreamHeader, StreamKind, StreamOpener,
};
use num_complex::Complex32;

use super::budget::{ChainKind, Slot};
use super::listen::{ListenConfig, SegmentFn, listen_class};
use super::{ChainReader, Next};
use crate::config::ListenSettings;
use crate::run::Shared;
use crate::stats::{ChainStatGuard, Counters, inc};

/// A channel narrower than this is floored to it (avoids a degenerate zero-width DDC spec), the
/// same floor `recipes::runtime::channel_plan` uses.
const MIN_BANDWIDTH_HZ: f64 = 1_000.0;

/// Publisher queue, bytes: must hold the header's `max_frame_len` (its default,
/// `hk_stream::header::DEFAULT_MAX_FRAME_LEN` = 1 MiB) plus its header and a marker, with
/// headroom for more than one buffered record.
const IQ_QUEUE_BYTES: usize = 2 * 1024 * 1024;

static STREAM_SEQ: AtomicU64 = AtomicU64::new(1);

/// `(lo, hi)` is inside a window tuned at `(center, rate)`.
fn in_window(center: f64, rate: f64, lo: f64, hi: f64) -> bool {
    rate.is_finite() && rate > 0.0 && lo >= center - 0.49 * rate && hi <= center + 0.49 * rate
}

/// The refusal for a request whose segment is ending: `503 replumbing` while the run continues in
/// a new window (retry), `410 source-ended` once the run is over.
fn segment_ended(continues: bool) -> OpenRefusal {
    if continues {
        OpenRefusal::new(
            503,
            "replumbing",
            "the run is moving to a new window; try again",
        )
    } else {
        OpenRefusal::new(410, "source-ended", "the source has ended")
    }
}

/// Little-endian `cf32_le` payload of `samples` (re, im pairs). Reuses `out`'s capacity.
fn encode(samples: &[Complex32], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(samples.len() * 8);
    for z in samples {
        out.extend_from_slice(&z.re.to_le_bytes());
        out.extend_from_slice(&z.im.to_le_bytes());
    }
}

/// The DDC spec for the band `[f_lo_hz, f_hi_hz]` under a window tuned at `tune_center_hz`. The
/// same construction the opener uses and its usefulness test exercises directly
/// ([`tests::the_ddc_carries_the_in_band_tone_and_rejects_the_out_of_band_one`]).
fn channel_spec(tune_center_hz: f64, f_lo_hz: f64, f_hi_hz: f64) -> DdcSpec {
    let bandwidth_hz = (f_hi_hz - f_lo_hz).max(MIN_BANDWIDTH_HZ);
    let center_hz = 0.5 * (f_lo_hz + f_hi_hz);
    DdcSpec::new(center_hz - tune_center_hz, bandwidth_hz)
}

/// Removes the chain's slot when dropped (the consumer went away or stopped): the run's on-demand
/// budget frees at once, even before the chain thread notices `stop` at its next read.
struct StopOnDrop {
    stop: Arc<AtomicBool>,
    slot: Slot,
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.slot.release();
    }
}

/// Why a chain ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Client,
    Idle,
    Retune,
    Segment,
    Source,
    Error,
}

/// Opens channelised IQ streams on a running pipeline (`PipelineHandle::iq_service`).
pub struct IqTapOpener {
    counters: Arc<Counters>,
    segment: SegmentFn,
    /// The run's on-demand limits (T-071 budget) and idle/backlog settings, read at each request
    /// (shared with Listen and the burst taps).
    settings: Arc<std::sync::Mutex<ListenSettings>>,
}

impl IqTapOpener {
    pub(crate) fn new(
        counters: Arc<Counters>,
        segment: SegmentFn,
        settings: Arc<std::sync::Mutex<ListenSettings>>,
    ) -> Self {
        Self {
            counters,
            segment,
            settings,
        }
    }

    fn open_inner(&self, req: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        let cfg = ListenConfig::from_settings(
            &self.settings.lock().unwrap_or_else(PoisonError::into_inner),
        );
        let target = IqTarget::from_request(req)?;
        let shared = (self.segment)().ok_or_else(|| {
            OpenRefusal::new(
                503,
                "replumbing",
                "the run is changing window or has ended; try again",
            )
        })?;
        let shared = &shared;
        if shared.ring.is_closed() || shared.stop.load(Ordering::SeqCst) {
            return Err(segment_ended(shared.continues.load(Ordering::SeqCst)));
        }
        let not_found = |what: &str| OpenRefusal::new(404, "not-found", format!("no such {what}"));
        let (lo, hi, emitter) = match target {
            IqTarget::Range { f_lo_hz, f_hi_hz } => (f_lo_hz, f_hi_hz, None),
            IqTarget::Emitter(id) => {
                let e = shared
                    .repo()
                    .emitter(id)
                    .map_err(|_| not_found("emitter"))?;
                let h = 0.5 * e.bandwidth_hz.max(0.0);
                (e.f_center_hz - h, e.f_center_hz + h, Some(id))
            }
        };
        if !(hi > lo && lo.is_finite() && hi.is_finite()) {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                "the target has no bandwidth",
            ));
        }
        if hi - lo > MAX_IQ_SPAN_HZ {
            return Err(OpenRefusal::new(
                400,
                "bad-request",
                format!("channel wider than {} MHz", MAX_IQ_SPAN_HZ / 1e6),
            ));
        }
        // Legal gate first: nothing reads the ring for a refused extent (see the module docs).
        let class = listen_class(
            shared.cfg.source_class,
            &shared.cfg.settings.classify,
            lo,
            hi,
        )?;
        // Admission (T-071): costed like a Listen chain from the tuned rate (the DDC's first
        // filter stage runs at the input rate regardless of how much the channel decimates).
        let rate_now = Some(shared.counters.tune().1)
            .filter(|r| r.is_finite() && *r > 0.0)
            .unwrap_or(shared.fs);
        let slot = Slot::claim(
            &self.counters,
            &cfg.limits(),
            ChainKind::Tap,
            cfg.chain_mcores(rate_now, Some(false)),
            rate_now,
        )?;
        let (center, rate) = shared.counters.tune();
        if !in_window(center, rate, lo, hi) {
            return Err(OpenRefusal::new(
                409,
                "outside-window",
                "the selection is not inside the tuned window",
            ));
        }
        let center_hz = 0.5 * (lo + hi);
        let bandwidth_hz = (hi - lo).max(MIN_BANDWIDTH_HZ);
        let spec = channel_spec(center, lo, hi);
        let ddc = Ddc::new(spec, rate).map_err(|_| {
            OpenRefusal::new(
                422,
                "unrealisable",
                "the channel cannot be down-converted to a streamable rate at the tuned rate",
            )
        })?;
        let stat = self.counters.chain_stats.register("iq-tap");
        let mut header = StreamHeader::new(
            format!("iq/{}", STREAM_SEQ.fetch_add(1, Ordering::Relaxed)),
            StreamKind::Iq,
            class,
            "hk-pipeline:iq-tap",
        );
        header.datatype = Some(IQ_DATATYPE.into());
        header.sample_rate_hz = Some(ddc.output_rate_hz());
        header.center_hz = Some(center_hz);
        header.bandwidth_hz = Some(bandwidth_hz);
        header.emitter_id = emitter;
        // Raw IQ chunks are far bigger than Listen's 20 ms audio frames (worst case: a full
        // ChainReader buffer un-decimated, `cf32_le`), so the queue must hold the header's
        // `max_frame_len` (its default) plus headroom, not Listen's small `cfg.queue_bytes`.
        let publisher = Publisher::new(
            header.clone(),
            PublisherConfig {
                queue_bytes: IQ_QUEUE_BYTES,
                disconnect_after_drops: u64::MAX,
                disconnect_after: Duration::from_secs(5),
                max_consumers: 1,
                drain_timeout: Duration::from_millis(500),
            },
        )
        .map_err(|e| OpenRefusal::new(500, "publisher", e.to_string()))?;
        let handle = publisher.handle();
        stat.set_channel(center_hz, bandwidth_hz);
        stat.set_stream(&header.stream_id, handle.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let start = shared.ring.next_sample().unwrap_or(0);
        let cursor = shared.gate.register(start);
        let reader = ChainReader::new(Arc::clone(shared), start, cursor).with_stat(stat.stat());
        let session = Session {
            shared: Arc::clone(shared),
            reader,
            ddc,
            publisher,
            handle: handle.clone(),
            stop: Arc::clone(&stop),
            tune: (center, rate),
            lo,
            hi,
            center_hz,
            idle_timeout: cfg.idle_timeout,
            max_backlog_s: cfg.max_backlog_s,
            scratch: Vec::new(),
            _slot: slot.clone(),
            stat,
        };
        if let Err(e) = thread::Builder::new()
            .name("hk-iq".into())
            .spawn(move || session.run())
        {
            return Err(OpenRefusal::new(500, "spawn", e.to_string()));
        }
        inc(&shared.counters.iq.attached);
        Ok(OpenedStream {
            header,
            handle,
            session: Box::new(StopOnDrop { stop, slot }),
        })
    }
}

impl StreamOpener for IqTapOpener {
    fn open(&self, request: &OpenRequest) -> Result<OpenedStream, OpenRefusal> {
        inc(&self.counters.iq.requests);
        let result = self.open_inner(request);
        if let Err(e) = &result {
            inc(&self.counters.iq.refused);
            if crate::debug_enabled() {
                eprintln!("hk-pipeline: iq tap refused: {e}");
            }
        }
        result
    }

    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "iq",
            "datatype": IQ_DATATYPE,
            "params": ["emitter", "f_lo", "f_hi"],
            "records": "one binary data record (type 1) per processed chunk: cf32_le re,im pairs, \
                        one per baseband sample of the requested channel; no mode or parameter, \
                        raw IQ is never demodulated",
        })
    }
}

struct Session {
    shared: Arc<Shared>,
    reader: ChainReader,
    ddc: Ddc,
    publisher: Publisher,
    handle: PublisherHandle,
    stop: Arc<AtomicBool>,
    /// Tuned centre and rate the DDC was built for.
    tune: (f64, f64),
    /// The requested band (fixed for the life of the chain; the DDC is rebuilt around it on a
    /// retune, never re-targeted).
    lo: f64,
    hi: f64,
    center_hz: f64,
    idle_timeout: Duration,
    max_backlog_s: f64,
    /// `cf32_le` encode scratch, reused every record.
    scratch: Vec<u8>,
    _slot: Slot,
    /// The chain's own counters (T-071).
    stat: ChainStatGuard,
}

impl Session {
    fn run(mut self) {
        let mut gap = true;
        let mut iq_index: u64 = 0;
        let mut last_consumer = Instant::now();
        let end = loop {
            if self.stop.load(Ordering::SeqCst) {
                break End::Client;
            }
            if self.handle.open_consumers() > 0 {
                last_consumer = Instant::now();
            } else if last_consumer.elapsed() > self.idle_timeout {
                break End::Idle;
            }
            match self.reader.next() {
                Next::Data(chunk) => {
                    let tune_now = (
                        chunk.provenance.tune.center_hz,
                        chunk.provenance.tune.sample_rate_hz,
                    );
                    if tune_now != self.tune {
                        if !in_window(tune_now.0, tune_now.1, self.lo, self.hi) {
                            break End::Retune;
                        }
                        let mut spec = self.ddc.spec().clone();
                        spec.center_offset_hz = self.center_hz - tune_now.0;
                        match Ddc::new(spec, tune_now.1) {
                            Ok(d) => {
                                self.ddc = d;
                                self.tune = tune_now;
                                gap = true;
                            }
                            Err(_) => break End::Error,
                        }
                    }
                    if !self.shared.gate.enabled() {
                        let head = self.shared.ring.next_sample().unwrap_or(0);
                        let behind = head.saturating_sub(chunk.end_sample());
                        if behind as f64 / self.shared.fs > self.max_backlog_s {
                            let cursor = self.shared.gate.register(head);
                            self.reader = ChainReader::new(Arc::clone(&self.shared), head, cursor)
                                .with_stat(self.stat.stat());
                            gap = true;
                            continue;
                        }
                    }
                    let anchor = (chunk.time.sample_index, chunk.time.host_time);
                    let info = InputInfo::from(&chunk);
                    let block = match self.ddc.process(info, &self.reader.buf[..chunk.len]) {
                        Ok(b) => b,
                        Err(_) => break End::Error,
                    };
                    if !block.samples.is_empty() {
                        let src_index = block.header.time.source_index;
                        let t = anchor.1.saturating_add_nanos(
                            ((src_index - anchor.0 as f64) / self.shared.fs * 1e9).round() as i64,
                        );
                        let n = block.samples.len();
                        encode(block.samples, &mut self.scratch);
                        let flags = if gap || block.header.discontinuity != Discontinuity::NONE {
                            RecordFlags::DISCONTINUITY
                        } else {
                            RecordFlags::empty()
                        };
                        match self.publisher.publish_binary(BinaryRecord {
                            t,
                            sample_index: iq_index,
                            flags,
                            payload: &self.scratch,
                        }) {
                            Ok(_) => {
                                gap = false;
                                inc(&self.stat.records);
                                inc(&self.shared.counters.iq.records);
                            }
                            Err(hk_stream::StreamError::ContentGated { .. }) => {
                                inc(&self.shared.counters.iq.gated);
                            }
                            Err(_) => {
                                inc(&self.shared.counters.iq.errors);
                            }
                        }
                        iq_index += n as u64;
                    }
                    self.reader.release_to(chunk.end_sample());
                }
                Next::Lost => gap = true,
                Next::Idle => {}
                Next::Closed => {
                    if self.shared.continues.load(Ordering::SeqCst) {
                        break End::Segment;
                    }
                    break End::Source;
                }
            }
        };
        self._slot.release();
        inc(&self.shared.counters.iq.detached);
        if crate::debug_enabled() {
            eprintln!(
                "hk-pipeline: iq {} detached ({end:?})",
                self.publisher.header().stream_id
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_core::{ProvenanceHandle, ReadChunk};
    use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};

    #[test]
    fn segment_ended_is_503_replumbing_or_410_source_ended() {
        let r = segment_ended(true);
        assert_eq!((r.status, r.code.as_str()), (503, "replumbing"));
        let r = segment_ended(false);
        assert_eq!((r.status, r.code.as_str()), (410, "source-ended"));
    }

    #[test]
    fn channel_spec_centres_on_the_requested_band_and_floors_bandwidth() {
        let spec = channel_spec(100e6, 100.15e6, 100.25e6);
        assert!((spec.center_offset_hz - 200_000.0).abs() < 1e-6);
        assert!((spec.bandwidth_hz - 100_000.0).abs() < 1e-6);

        // A degenerate (zero-width) request is floored, not rejected with a NaN/zero spec.
        let spec = channel_spec(100e6, 100.2e6, 100.2e6);
        assert!((spec.bandwidth_hz - MIN_BANDWIDTH_HZ).abs() < 1e-6);
    }

    #[test]
    fn cf32_le_encoding_round_trips() {
        let mut out = Vec::new();
        encode(&[Complex32::new(1.0, -2.5)], &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(&out[..4], &1.0f32.to_le_bytes());
        assert_eq!(&out[4..], &(-2.5f32).to_le_bytes());
    }

    fn provenance(tune: (f64, f64)) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "synthetic:t165".into(),
            tune: Tune {
                center_hz: tune.0,
                sample_rate_hz: tune.1,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: tune.1,
            },
            quantisation_limited: false,
            overload: false,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: Some(0),
        })
    }

    /// `len` complex samples at `prov`'s tune carrying the sum of `tones_hz` (absolute Hz),
    /// quantised like a real ring's `ci8` (hk-core normalises by `/128`, see
    /// `hk_dsp::stft::IqSample for Complex<i8>`).
    fn two_tone_chunk(
        prov: &ProvenanceHandle,
        first: u64,
        len: usize,
        tones_hz: [f64; 2],
    ) -> (ReadChunk, Vec<num_complex::Complex<i8>>) {
        let fs = prov.tune.sample_rate_hz;
        let fc = prov.tune.center_hz;
        let x = (0..len as u64)
            .map(|i| {
                let t = (first + i) as f64;
                let (mut re, mut im) = (0.0f64, 0.0f64);
                for f_abs in tones_hz {
                    let ph = std::f64::consts::TAU * (f_abs - fc) * t / fs;
                    re += ph.cos();
                    im += ph.sin();
                }
                num_complex::Complex::new(
                    (40.0 * re).round().clamp(-120.0, 120.0) as i8,
                    (40.0 * im).round().clamp(-120.0, 120.0) as i8,
                )
            })
            .collect();
        let c = ReadChunk {
            time: SampleTime {
                sample_index: first,
                host_time: Timestamp::from_unix_nanos(0),
            },
            len,
            block_start: first == 0,
            discontinuity: if first == 0 {
                Discontinuity::STREAM_START
            } else {
                Discontinuity::NONE
            },
            dropped_before: 0,
            provenance: prov.clone(),
        };
        (c, x)
    }

    /// Power at baseband frequency `f_hz` over `samples` at `rate_hz` (a single-frequency DFT
    /// term, i.e. a Goertzel evaluation): proportional to the true tone power there, up to a
    /// fixed scale shared by every frequency it is compared against.
    fn power_at(samples: &[Complex32], rate_hz: f64, f_hz: f64) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (k, s) in samples.iter().enumerate() {
            let ph = -std::f64::consts::TAU * f_hz * k as f64 / rate_hz;
            let (sph, cph) = ph.sin_cos();
            re += f64::from(s.re) * cph - f64::from(s.im) * sph;
            im += f64::from(s.re) * sph + f64::from(s.im) * cph;
        }
        (re * re + im * im) / (samples.len() as f64).powi(2)
    }

    /// **Usefulness test (T-160/T-162 standard): proves the served IQ is actually the requested
    /// channel**, not just correctly shaped. Two tones sit in a 2 MHz-wide capture: one inside
    /// the requested 100 kHz-wide band (well inside its ±50 kHz passband edge) and one 500 kHz
    /// outside it, past the DDC's transition band. The same [`channel_spec`] the opener builds is
    /// fed straight into a real [`Ddc`] (exactly `IqTapOpener::open_inner`'s construction; the DDC
    /// engine's own tone/amplitude/phase math is proved in `hk-dsp`'s
    /// `channelizer_ddc.rs`/`ddc_review.rs`, so this test is about *this opener's wiring*, not
    /// re-deriving that). The in-band tone must land at its expected baseband offset with a wide
    /// margin over everywhere else in the output band, including wherever the rejected
    /// out-of-band tone would have folded to had the channel selection been wrong.
    #[test]
    fn the_ddc_carries_the_in_band_tone_and_rejects_the_out_of_band_one() {
        let tune = (100_000_000.0, 2_000_000.0);
        let prov = provenance(tune);
        let (f_lo, f_hi) = (tune.0 + 150_000.0, tune.0 + 250_000.0); // channel centre +200 kHz
        let spec = channel_spec(tune.0, f_lo, f_hi);
        let expected_offset_hz = -spec.center_offset_hz; // baseband freq of a tone at tune centre
        let mut ddc = Ddc::new(spec, tune.1).expect("realisable");

        let in_band_hz = tune.0 + 240_000.0; // 40 kHz off channel centre: well inside the passband
        let out_of_band_hz = tune.0 + 700_000.0; // past the transition band entirely

        let mut out: Vec<Complex32> = Vec::new();
        let (chunk_len, total) = (4096usize, 50_000usize);
        let mut pos = 0u64;
        while (pos as usize) < total {
            let n = chunk_len.min(total - pos as usize);
            let (c, x) = two_tone_chunk(&prov, pos, n, [in_band_hz, out_of_band_hz]);
            let b = ddc.process(InputInfo::from(&c), &x).unwrap();
            out.extend_from_slice(b.samples);
            pos += n as u64;
        }
        assert!(out.len() > 1000, "the DDC produced output: {}", out.len());

        let rate = ddc.output_rate_hz();
        let expected_baseband_hz = (in_band_hz - tune.0) + expected_offset_hz;
        let p_tone = power_at(&out, rate, expected_baseband_hz);

        // Sweep the whole output Nyquist range for anything comparable to the expected tone,
        // including wherever the out-of-band tone (or a wiring bug swapping lo/hi) would show up.
        let bin_hz = rate / out.len() as f64;
        let mut worst_other = f64::MIN;
        let steps = 64;
        for k in 0..=steps {
            let f = -0.5 * rate + rate * (k as f64 / steps as f64);
            if (f - expected_baseband_hz).abs() > 3.0 * bin_hz {
                worst_other = worst_other.max(power_at(&out, rate, f));
            }
        }
        let margin_db = 10.0 * (p_tone / worst_other).log10();
        assert!(
            margin_db > 20.0,
            "the requested channel's tone ({p_tone}) should stand far above everywhere else in \
             the served band, including where the out-of-band tone the DDC must reject would \
             land ({worst_other}); margin {margin_db} dB"
        );
    }
}
