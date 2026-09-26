//! A scripted, retunable receiver behind the generic device contract, for control-plane tests
//! (T-050, T-057). It generates IQ from a closure of the tuned window, applies tune, rate and gain
//! changes at block boundaries like a radio (new provenance, `RETUNE`/`RATE_CHANGE` flags), holds
//! at a sample index (empty blocks, so the capture thread keeps checking its stop flag), and ends
//! on command. It is pausable (read on demand), so tests run lossless and deterministic.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hk_core::source::SampleRates;
use hk_core::{
    BlockHeader, ControlMailbox, Discontinuity, Gains, ProvenanceHandle, Source,
    SourceCapabilities, SourceControl, SourceError,
};
use hk_model::{ClockSource, Provenance, SampleTime, Timestamp, TimestampMethod, Tune};
use num_complex::{Complex, Complex32};

/// Fills `out` with `n` samples for the window `(center_hz, rate_hz)` starting at stream index
/// `index`.
pub type Generator = Box<dyn FnMut(f64, f64, u64, usize, &mut Vec<Complex<i8>>) + Send>;

/// 2026-09-13T12:00:00Z, ns.
pub const T0_NS: i64 = 1_789_300_800_000_000_000;

/// The radio's control handle (and the test's script).
pub struct RadioControl {
    caps: SourceCapabilities,
    mailbox: ControlMailbox,
    hold_at: AtomicU64,
    finish: AtomicBool,
    emitted: AtomicU64,
    /// T-497: accept `tune` and never apply it (see [`RadioControl::lose_center`]).
    lose_center: AtomicBool,
    /// T-508: every read fails (see [`RadioControl::fail_reads`]).
    fail_reads: AtomicBool,
    /// T-541: reads still to fail before the front end comes back (see
    /// [`RadioControl::fail_reads_for`]).
    fail_reads_for: AtomicU64,
    /// T-541: rate changes still to refuse (see [`RadioControl::refuse_rate`]).
    refuse_rate: AtomicU64,
    /// T-541: blocks still to deliver with impossible provenance (see [`RadioControl::garbage`]).
    garbage: AtomicU64,
    /// T-525: reads that do not look at the control mailbox (see
    /// [`RadioControl::hold_changes`]).
    hold_changes: AtomicU64,
    /// T-939: blocks between dropped spans (see [`RadioControl::drop_every`]).
    drop_every: AtomicU64,
    /// T-939: samples dropped each time.
    drop_samples: AtomicU64,
    /// Blocks delivered (the drop cadence counts these, not samples).
    delivered: AtomicU64,
    /// `(stream index of the first block, centre, rate)` for every applied window change.
    pub windows: Mutex<Vec<(u64, f64, f64)>>,
    /// Receive-side control calls, in order.
    pub calls: Mutex<Vec<String>>,
}

impl RadioControl {
    /// Delivers samples up to stream index `index`, then holds.
    pub fn hold_at(&self, index: u64) {
        self.hold_at.store(index, Ordering::SeqCst);
    }

    /// Streams without holding.
    pub fn run_free(&self) {
        self.hold_at.store(u64::MAX, Ordering::SeqCst);
    }

    /// **Drops `samples` samples before every `blocks`-th block** (T-939), the way a USB front end
    /// that the host did not drain in time does: the stream index jumps over them and the block
    /// carrying the jump reports them in `dropped_before`.
    ///
    /// Deterministic in stream terms — the drops fall on block counts, not on the wall clock — so
    /// a gate test can state exactly how often the stream breaks. `blocks = 0` turns it off.
    pub fn drop_every(&self, blocks: u64, samples: u64) {
        self.drop_every.store(blocks, Ordering::SeqCst);
        self.drop_samples.store(samples, Ordering::SeqCst);
    }

    /// Ends the stream (the next read returns `Ok(None)`).
    pub fn finish(&self) {
        self.finish.store(true, Ordering::SeqCst);
    }

    /// Samples delivered so far (the next stream index).
    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::SeqCst)
    }

    /// **Accept `tune` and never apply it** — a front end that takes the command and does not move
    /// (T-497).
    ///
    /// This is not a contrived fault. `hk_core`'s HackRF driver applies a posted control **field by
    /// field and returns on the first `SourceError`** (`apply_change`: sample rate, then baseband
    /// filter, then gains, then centre), so a baseband-filter write that fails leaves the new
    /// *rate* in the block provenance and the centre never applied. Every block then reports a
    /// window one component away from the one the re-plumb asked for — which is the state
    /// [`hk_pipeline::run::WINDOW_SETTLE_TIMEOUT`] exists to get out of, and which no mock SDR can
    /// produce, because a mock echoes whatever it was told.
    ///
    /// The call is still logged, so a test can see the front end *was* commanded.
    pub fn lose_center(&self, on: bool) {
        self.lose_center.store(on, Ordering::SeqCst);
    }

    /// **A slow control path: the next `n` reads do not look at the mailbox** (T-525).
    ///
    /// Not a contrived fault either — it is the ordinary shape of a front end whose control writes
    /// take longer than a block. [`hk_core::ControlMailbox`] **coalesces**: `take` returns every
    /// change posted since the last one, with later fields overwriting earlier ones. So while a
    /// posted change is waiting to be taken, a second change to the same field *replaces* it and
    /// the first never reaches the tuning — the device goes straight to the second window and
    /// never reports the first. A segment told to expect the first then waits for a window that
    /// no longer exists, which before T-525 could only end at
    /// [`hk_pipeline::WINDOW_SETTLE_TIMEOUT`].
    ///
    /// Counted in reads rather than seconds so the test is deterministic: this radio is pausable
    /// and read on demand, so "n reads" is a fact about the script, not about the clock.
    pub fn hold_changes(&self, n: u64) {
        self.hold_changes.store(n, Ordering::SeqCst);
    }

    /// **Every read fails with a device error** (T-508) — a front end that is gone: unplugged, or
    /// a USB stall the driver reports as `SourceError::Device` (`hackrf.rs`'s `check_stall`).
    pub fn fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::SeqCst);
    }

    /// **T-541: a read error that CLEARS** — the next `n` reads fail with a device error and the
    /// front end then delivers again.
    ///
    /// [`RadioControl::fail_reads`] models a device that is gone for good, which can only prove
    /// that the run ends honestly. The far more common fault is the one that passes: a USB stall,
    /// a transfer timeout, a retune the driver could not apply on the first try. It is the shape
    /// that has to leave the run *back on `running`* afterwards, because a system that answers
    /// every hiccup by ending the run is as unusable as one that crashes.
    ///
    /// Counted in reads, not seconds, so the test is deterministic: this radio is pausable and
    /// read on demand.
    pub fn fail_reads_for(&self, n: u64) {
        self.fail_reads_for.store(n, Ordering::SeqCst);
    }

    /// **T-541: the device refuses the next `n` sample-rate changes** — `set_sample_rate` returns
    /// [`SourceError::Device`] instead of posting to the mailbox.
    ///
    /// Different from [`RadioControl::fail_reads`] and from T-508's mock fault, both of which
    /// fail on the *capture* thread: this one fails on the **control** thread, inside the caller's
    /// own `retune`. It is what a HackRF does when `hackrf_set_sample_rate` returns non-zero, and
    /// it is the one device error the user is holding a request open for, so it must come back as
    /// a refusal to *that caller* and leave the run capturing on the window it was already on —
    /// never a run that ends because a control call failed.
    pub fn refuse_rate(&self, n: u64) {
        self.refuse_rate.store(n, Ordering::SeqCst);
    }

    /// **T-541: a front end that returns garbage** — the next `n` blocks carry provenance that
    /// cannot be true: a **zero sample rate**, a **NaN centre**, and a sample index that jumps
    /// **backwards**.
    ///
    /// Not contrived. Every one of these is a plausible read of a corrupt USB transfer or an
    /// uninitialised driver struct, and every one lands on arithmetic the capture thread does per
    /// block: `n / fs` for the block's end time, the axis splice, the ring write, and downstream
    /// the history pyramid's monotone watermark. A divide by zero on floats is an infinity rather
    /// than a trap, which is exactly why this needs a test rather than an argument — the question
    /// is not whether it panics but whether anything downstream turns the infinity into an index.
    pub fn garbage(&self, n: u64) {
        self.garbage.store(n, Ordering::SeqCst);
    }

    /// Waits until `emitted() >= n`; `false` after `limit`.
    pub fn wait_emitted(&self, n: u64, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while self.emitted() < n {
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    fn log(&self, s: String) {
        self.calls.lock().unwrap().push(s);
    }
}

impl SourceControl for RadioControl {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.caps
    }

    fn tune(&self, center_hz: f64) -> Result<(), SourceError> {
        if !self.caps.supports_frequency(center_hz) {
            return Err(SourceError::OutOfRange {
                what: "centre frequency (Hz)",
                value: center_hz,
            });
        }
        self.log(format!("tune {center_hz}"));
        // T-497: a front end that takes the command and does not move. The call is logged either
        // way, so "the radio was told" and "the radio went" stay separable.
        if !self.lose_center.load(Ordering::SeqCst) {
            self.mailbox.post(|p| p.center_hz = Some(center_hz));
        }
        Ok(())
    }

    fn set_sample_rate(&self, rate: f64) -> Result<(), SourceError> {
        if !self.caps.sample_rates.supports(rate) {
            return Err(SourceError::OutOfRange {
                what: "sample rate (Hz)",
                value: rate,
            });
        }
        self.log(format!("rate {rate}"));
        // T-541: the device refuses the change on the control thread. Logged first, so "the radio
        // was told" and "the radio accepted" stay separable, as with `lose_center`.
        if self
            .refuse_rate
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok()
        {
            return Err(SourceError::Device {
                source_name: "scripted-radio",
                operation: "set_sample_rate",
                message: format!("the device refused {rate} Hz (scripted)"),
            });
        }
        self.mailbox.post(|p| p.sample_rate_hz = Some(rate));
        Ok(())
    }

    fn set_gains(&self, gains: &Gains) -> Result<(), SourceError> {
        self.log(format!("gains {gains:?}"));
        let g = *gains;
        self.mailbox.post(|p| p.gains = Some(g));
        Ok(())
    }

    fn set_gain(&self, stage: &str, db: f64) -> Result<(), SourceError> {
        if self.caps.gain_stage(stage).is_none() {
            return Err(SourceError::OutOfRange {
                what: "gain stage",
                value: db,
            });
        }
        self.log(format!("gain {stage} {db}"));
        Ok(())
    }

    fn set_baseband_filter(&self, hz: f64) -> Result<(), SourceError> {
        self.log(format!("filter {hz}"));
        self.mailbox.post(|p| p.baseband_filter_hz = Some(hz));
        Ok(())
    }

    fn set_bias_tee(&self, on: bool) -> Result<(), SourceError> {
        self.log(format!("bias {on}"));
        Ok(())
    }

    fn start(&self) -> Result<(), SourceError> {
        Ok(())
    }

    fn stop(&self) -> Result<(), SourceError> {
        self.finish();
        Ok(())
    }
}

/// The radio's stream.
pub struct Radio {
    control: Arc<RadioControl>,
    generate: Generator,
    provenance: ProvenanceHandle,
    seen: u64,
    t_ns: i64,
    block: usize,
    started: bool,
    pending: Discontinuity,
}

/// HackRF-One-like capabilities (TX-capable hardware, bias tee, named gains) with rates from
/// 200 kS/s, so tests can run narrow windows.
pub fn capabilities() -> SourceCapabilities {
    let mut caps = SourceCapabilities::hackrf_one();
    caps.driver = "scripted-radio".into();
    caps.sample_rates = SampleRates::Continuous {
        min_hz: 200e3,
        max_hz: 20e6,
    };
    caps
}

impl Radio {
    /// A radio tuned to `(center_hz, rate_hz)` delivering `block`-sample blocks.
    pub fn new(
        center_hz: f64,
        rate_hz: f64,
        block: usize,
        generate: Generator,
    ) -> (Self, Arc<RadioControl>) {
        let control = Arc::new(RadioControl {
            caps: capabilities(),
            mailbox: ControlMailbox::new(),
            hold_at: AtomicU64::new(u64::MAX),
            finish: AtomicBool::new(false),
            emitted: AtomicU64::new(0),
            lose_center: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            fail_reads_for: AtomicU64::new(0),
            refuse_rate: AtomicU64::new(0),
            drop_every: AtomicU64::new(0),
            drop_samples: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            garbage: AtomicU64::new(0),
            hold_changes: AtomicU64::new(0),
            windows: Mutex::new(vec![(0, center_hz, rate_hz)]),
            calls: Mutex::new(Vec::new()),
        });
        let provenance = ProvenanceHandle::new(Provenance {
            device_id: "scripted:radio".into(),
            tune: Tune {
                center_hz,
                sample_rate_hz: rate_hz,
                lna_db: 16.0,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 0.75 * rate_hz,
            },
            overload: false,
            quantisation_limited: false,
            noise_sigma_lsb: None,
            temperature_c: None,
            antenna_port: None,
            bias_tee: hk_model::BiasTee::Unknown,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Synthetic,
            timestamp_error_budget_ns: None,
            capture_artefacts: Vec::new(),
        });
        (
            Self {
                control: Arc::clone(&control),
                generate,
                provenance,
                seen: 0,
                t_ns: T0_NS,
                block,
                started: false,
                pending: Discontinuity::NONE,
            },
            control,
        )
    }

    /// T-942: the same radio, its first sample stamped `t_ns` — a **restart** resumes later in
    /// capture time than the run before it, which is the whole shape of the defect.
    #[must_use]
    pub fn starting_at(mut self, t_ns: i64) -> Self {
        self.t_ns = t_ns;
        self
    }
}

impl Source for Radio {
    fn capabilities(&self) -> &SourceCapabilities {
        &self.control.caps
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        Arc::clone(&self.control) as Arc<dyn SourceControl>
    }

    fn pausable(&self) -> bool {
        true
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let mut ci8 = Vec::new();
        let h = self.read_block_ci8(&mut ci8)?;
        samples.clear();
        samples.extend(
            ci8.iter()
                .map(|z| Complex32::new(f32::from(z.re) / 128.0, f32::from(z.im) / 128.0)),
        );
        Ok(h)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        samples.clear();
        let c = &self.control;
        if c.finish.load(Ordering::SeqCst) {
            return Ok(None);
        }
        // T-541: a transient stall consumes one of its budget and then clears by itself.
        let transient = c
            .fail_reads_for
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok();
        if transient || c.fail_reads.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
            return Err(SourceError::Device {
                source_name: "scripted-radio",
                operation: "receive",
                message: if transient {
                    "a USB transfer stalled (scripted, clears)".into()
                } else {
                    "the front end is gone (scripted)".to_owned()
                },
            });
        }
        let index = c.emitted();
        // T-525: a control path that takes longer than a block. The mailbox is left alone, so a
        // change posted behind one already waiting coalesces over it.
        let held = c
            .hold_changes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n > 0).then(|| n - 1)
            })
            .is_ok();
        if let Some(p) = (!held).then(|| c.mailbox.take(&mut self.seen)).flatten() {
            let mut next = self.provenance.get().clone();
            p.apply_to(&mut next.tune);
            let flags = Discontinuity::between(self.provenance.get(), &next);
            if !flags.is_empty() {
                c.windows.lock().unwrap().push((
                    index,
                    next.tune.center_hz,
                    next.tune.sample_rate_hz,
                ));
                self.provenance = ProvenanceHandle::new(next);
                self.pending =
                    Discontinuity::from_bits_truncate(self.pending.bits() | flags.bits());
            }
        }
        // T-939: a front end that drops. The stream index jumps over the dropped span and the
        // clock with it; everything after is generated at the new index, so the samples stay
        // continuous in content and only the stream is broken.
        let every = c.drop_every.load(Ordering::SeqCst);
        let dropped =
            if every > 0 && c.delivered.fetch_add(1, Ordering::SeqCst) % every == every - 1 {
                c.drop_samples.load(Ordering::SeqCst)
            } else {
                0
            };
        let index = index + dropped;
        if dropped > 0 {
            self.t_ns +=
                (dropped as f64 * 1e9 / self.provenance.tune.sample_rate_hz).round() as i64;
        }
        let header = |d: Discontinuity, t_ns: i64, prov: &ProvenanceHandle| BlockHeader {
            time: SampleTime {
                sample_index: index,
                host_time: Timestamp::from_unix_nanos(t_ns),
            },
            provenance: prov.clone(),
            discontinuity: d,
            dropped_before: 0,
        };
        let hold = c.hold_at.load(Ordering::SeqCst);
        if index >= hold {
            std::thread::sleep(Duration::from_millis(1));
            return Ok(Some(header(
                Discontinuity::NONE,
                self.t_ns,
                &self.provenance,
            )));
        }
        let n = (self.block as u64).min(hold.saturating_sub(index)) as usize;
        let (center, rate) = (
            self.provenance.tune.center_hz,
            self.provenance.tune.sample_rate_hz,
        );
        (self.generate)(center, rate, index, n, samples);
        samples.truncate(n);
        let mut flags = std::mem::replace(&mut self.pending, Discontinuity::NONE);
        if !std::mem::replace(&mut self.started, true) {
            flags = Discontinuity::from_bits_truncate(
                flags.bits() | Discontinuity::STREAM_START.bits(),
            );
        }
        let mut h = header(flags, self.t_ns, &self.provenance);
        h.dropped_before = dropped;
        // T-541: a block whose provenance cannot be true. The samples are real (the generator
        // ran); it is the *description* of them that is corrupt, which is the interesting half —
        // an impossible rate or centre reaches the axis arithmetic, the coverage map and the
        // history watermark, while an empty block would reach none of it.
        if c.garbage
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |g| {
                (g > 0).then(|| g - 1)
            })
            .is_ok()
        {
            let mut bad = self.provenance.get().clone();
            bad.tune.sample_rate_hz = 0.0;
            bad.tune.center_hz = f64::NAN;
            h.provenance = ProvenanceHandle::new(bad);
            h.time.sample_index = index.saturating_sub(self.block as u64 * 4);
        }
        self.t_ns += (n as f64 * 1e9 / rate).round() as i64;
        c.emitted.store(index + n as u64, Ordering::SeqCst);
        Ok(Some(h))
    }
}

/// A complex tone at `offset_hz` (amplitude 40) in light noise, continuous in phase across calls.
pub fn tone(offset_hz: impl Fn(f64) -> f64 + Send + 'static) -> Generator {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    Box::new(move |center, rate, index, n, out| {
        let f = offset_hz(center);
        for i in 0..n as u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = ((state >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 8.0;
            let ph = 2.0 * std::f64::consts::PI * f * (index + i) as f64 / rate;
            out.push(Complex::new(
                (40.0 * ph.cos() + noise).round().clamp(-128.0, 127.0) as i8,
                (40.0 * ph.sin() - noise).round().clamp(-128.0, 127.0) as i8,
            ));
        }
    })
}

/// Plays `iq` in a loop (e.g. a synthetic scene), whatever the window.
pub fn looped(iq: Vec<Complex<i8>>) -> Generator {
    Box::new(move |_, _, index, n, out| {
        let len = iq.len() as u64;
        out.extend((0..n as u64).map(|i| iq[((index + i) % len) as usize]));
    })
}
