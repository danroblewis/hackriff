//! Always-on reader 3: the live spectrum stream (C24, ADR-0004).
//!
//! Its own STFT at the display FFT size and at most the display row rate ([`row_plan`]), PSD rows
//! in dBFS/Hz as little-endian f32 (`rf32_le`), published drop-not-block under the source class
//! ([`crate::class`]): a class that forbids content gates spectrum at ≤ 50 rows/s (the publisher
//! enforces it; withheld rows are counted). The publisher is offered to
//! [`crate::config::StreamSink`] (the hk-api bridge registry).
//!
//! - **Headers follow the rows (T-057).** A stream header is sent once per connection and must
//!   describe every row after it. Each row's own frame (its spectrum's centre and span, from the
//!   provenance the STFT carried) is compared with the header in force; when the centre, the
//!   rate/span, the FFT size or the declared row rate differ, the old publisher is finished and a
//!   new one with a matching header is offered under the same id **before** that row is
//!   published. This covers every tune path (the control API, the scheduler, a raw
//!   `SourceControl`, multi-capture recordings), not only blocks flagged `RETUNE`. The STFT resets
//!   on a centre, rate or gain change, so no row mixes two windows.
//! - **Display settings (T-050)** come from [`DisplayControl`] and apply between chunks without a
//!   restart: FFT size and row rate rebuild the STFT (and re-offer the header at the next row);
//!   `averaging` is an exponential moving average over published rows in linear power (1 = off,
//!   reset at every header change or STFT reset).
//!
//! **This reader never stops publishing on a viewer's account (T-347).** It used to, on a run-wide
//! `paused` flag, which meant one browser's Pause froze every other browser's waterfall. Pause is
//! the client's own time window now ([`crate::config::DisplaySettings`] says why), so the rows go
//! out for as long as the run does and a held view simply stops advancing over them.
//!
//! **And pausing is not where the battery goes (T-348).** Measured on `hk serve --replay` at
//! 2.4 Msps (fft 4096, 25 rows/s), steady state, no consumer, three interleaved 60 s windows of
//! process CPU: skipping this reader's `stft.push` entirely saves ~1.7 of ~11 CPU-seconds per 60 s
//! of wall clock, ~13% of the pipeline's CPU (~0.03 of ~0.18 cores). Essentially all of it is the
//! FFT — skipping only `Output::row` (the EMA, the dB conversion, the serialize, the publish)
//! saves nothing measurable at 25 rows/s, and neither does an attached consumer, which was drawing
//! 407 kB/s for free. **None of that 13% is reachable by pausing,** for two deliberate reasons:
//! there is no run-wide pause left to check (above), and the client keeps one socket per session
//! whatever its panes do, because T-445/T-457 put the live-edge capture clock, the tuned geometry
//! and the newest trace row on it — a frozen pane still needs the first two, so `open_consumers()`
//! stays ≥ 1 while any browser is open. The only lever on that 13% is therefore "nothing is
//! subscribed at all", which is the headless scheduled-survey case rather than the paused-screen
//! one; T-489 carries it with the hazards it has to handle. What can never be saved either way is
//! the ring read and the `cursor.set` below: reader 3 holds a gate cursor, so it drains the ring
//! whether or not anyone is looking, or a lossless run stalls capture behind it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_core::{Discontinuity, ReadOutcome};
use hk_dsp::{InputInfo, PowerUnit, SpectrumFrame, StftProcessor};
use hk_model::ContentClass;
use hk_store::history::{FrameOrigin, source_key};
use hk_stream::{BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamError};
use num_complex::Complex;

use crate::attention::AttentionService;
use crate::class::{RowPlan, row_plan, spectrum_header};
use crate::compute::Reader;
use crate::config::{DisplayPatch, DisplaySettings};
use crate::run::Shared;
use crate::stats::{add, inc, set};

/// The run's display settings, shared by the spectrum reader of every segment and the controller.
#[derive(Debug)]
pub(crate) struct DisplayControl {
    settings: Mutex<DisplaySettings>,
    generation: AtomicU64,
}

impl DisplayControl {
    pub fn new(settings: DisplaySettings) -> Self {
        Self {
            settings: Mutex::new(settings),
            generation: AtomicU64::new(0),
        }
    }

    /// The settings in force.
    pub fn get(&self) -> DisplaySettings {
        *self.settings.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes since start (the reader polls it between chunks).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Applies `patch` (all or nothing).
    pub fn patch(&self, patch: &DisplayPatch) -> Result<DisplaySettings, String> {
        let mut s = self.settings.lock().unwrap_or_else(PoisonError::into_inner);
        *s = s.patched(patch)?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(*s)
    }
}

/// What a header describes (compared exactly: every value comes from the same provenance/plan).
#[derive(Clone, Copy, Debug, PartialEq)]
struct HeaderKey {
    center_hz: f64,
    span_hz: f64,
    bins: usize,
    declared_hz: f64,
}

/// Publishes rows under a header that matches each row.
struct Output<'a> {
    shared: &'a Shared,
    class: ContentClass,
    plan: RowPlan,
    settings: DisplaySettings,
    publisher: Option<Publisher>,
    key: Option<HeaderKey>,
    db: Vec<f32>,
    bytes: Vec<u8>,
    avg: Vec<f32>,
    avg_rows: u32,
    error: Option<anyhow::Error>,
    /// T-484: the view lattice's writer queue, whose finest node these frames *are*.
    view: Option<Arc<crate::history::ViewQueue>>,
    /// T-133: the site each frame is folded under, peeked at the frame's sample time.
    attention: Option<Arc<AttentionService>>,
}

impl<'a> Output<'a> {
    fn new(
        shared: &'a Shared,
        plan: RowPlan,
        settings: DisplaySettings,
        attention: Option<Arc<AttentionService>>,
    ) -> Self {
        Self {
            shared,
            class: shared.cfg.source_class,
            plan,
            settings,
            publisher: None,
            key: None,
            db: Vec::new(),
            bytes: Vec::new(),
            avg: Vec::new(),
            avg_rows: 0,
            error: None,
            view: shared.view_queue.clone(),
            attention,
        }
    }

    /// A new plan: the next row gets a new header (the key includes bins and declared rate).
    fn set_plan(&mut self, plan: RowPlan) {
        self.plan = plan;
        self.avg_rows = 0;
    }

    /// Finishes the publisher in force and offers one whose header describes `key`.
    fn reoffer(&mut self, key: HeaderKey) -> Result<(), anyhow::Error> {
        if let Some(old) = self.publisher.take() {
            old.finish();
        }
        self.key = None;
        let header = spectrum_header(
            &self.shared.cfg.spectrum_stream_id,
            "hk-pipeline:spectrum",
            self.class,
            &self.plan,
            key.center_hz,
            key.span_hz,
        );
        let p = Publisher::new(
            header.clone(),
            PublisherConfig {
                queue_bytes: (1 << 20).max(64 * (32 + 4 * key.bins)),
                ..PublisherConfig::default()
            },
        )?;
        if let Some(sink) = &self.shared.cfg.stream_sink {
            sink(&header, p.handle());
        }
        self.publisher = Some(p);
        self.key = Some(key);
        Ok(())
    }

    fn row(&mut self, frame: &SpectrumFrame) {
        if self.error.is_some() {
            return;
        }
        let spec = &frame.spectrum;
        let bins = self.plan.stft.welch.fft_len;
        if spec.bins() != bins {
            return;
        }
        let key = HeaderKey {
            center_hz: spec.f_center_hz,
            span_hz: spec.sample_rate_hz,
            bins,
            declared_hz: self.plan.declared_hz,
        };
        let reset = frame.discontinuity.bits() & !Discontinuity::STREAM_START.bits() != 0;
        if reset || self.key != Some(key) {
            self.avg_rows = 0;
        }
        let averaging = self.settings.averaging.max(1);
        if averaging > 1 {
            if self.avg.len() != bins || self.avg_rows == 0 {
                self.avg.clear();
                self.avg.extend_from_slice(&spec.psd);
                self.avg_rows = 1;
            } else {
                self.avg_rows = (self.avg_rows + 1).min(averaging);
                let alpha = 1.0 / self.avg_rows as f32;
                for (a, &p) in self.avg.iter_mut().zip(&spec.psd) {
                    *a += alpha * (p - *a);
                }
            }
        }
        if self.key != Some(key) {
            if let Err(e) = self.reoffer(key) {
                self.error = Some(e);
                return;
            }
        }
        self.db.resize(bins, 0.0);
        self.bytes.resize(4 * bins, 0);
        let trace: &[f32] = if averaging > 1 { &self.avg } else { &spec.psd };
        spec.write_db(trace, PowerUnit::DbfsPerHz, &mut self.db);
        for (c, v) in self.bytes.chunks_exact_mut(4).zip(&self.db) {
            c.copy_from_slice(&v.to_le_bytes());
        }
        let mut flags = RecordFlags::empty();
        if frame.provenance.get().overload {
            flags = flags.with(RecordFlags::OVERLOAD);
        }
        if reset {
            flags = flags.with(RecordFlags::DISCONTINUITY);
        }
        // **T-484: the canvas's finest tier is THIS row.**
        //
        // The view lattice's node (0, 0) is sized to this plan's own bin and row
        // ([`crate::history::view_geometry`]), so the fold here is 1:1 — one FFT bin of one
        // published row per cell — and `/api/tiles`'s `max_db` at that node is the number this
        // reader just wrote into `self.db`, not a max-hold over ~10³ of them. T-483 measured what
        // the second STFT cost: +10.5 dB of floor lift, 5.0 dB of contrast on the 100.465 MHz
        // emission, and 3 % of the station's level variation retained.
        //
        // It is pushed **before** the publish and regardless of the class gate: the gate withholds
        // payloads from external consumers (`StreamError::SpectrumGated`), and this is the local
        // store, which scheme 1 already fills at full resolution from the history reader.
        //
        // It is `spec.psd`, not `trace`: `averaging` is an exponential moving average the *viewer*
        // asked for, and a display filter does not belong in the store. At the default
        // (`averaging = 1`) they are the same slice, which is the case T-483 compares.
        //
        // A clone and a push — no pyramid lock, no tile write, no zstd. This thread holds a gate
        // cursor; nothing here can be made slow by a reader or by the disk.
        if let Some(q) = self.view.as_ref() {
            q.push(
                &self.shared.counters.history,
                frame,
                FrameOrigin {
                    source: source_key(&frame.provenance.get().device_id),
                    site: Some(crate::history::frame_site(
                        self.attention.as_deref(),
                        frame.t.host_time,
                    )),
                },
            );
        }
        let sc = &self.shared.counters.spectrum;
        inc(&sc.rows);
        let p = self.publisher.as_mut().expect("publisher offered above");
        match p.publish_binary(BinaryRecord {
            t: frame.t.host_time,
            sample_index: frame.t.sample_index,
            flags,
            payload: &self.bytes,
        }) {
            Ok(_) => {}
            Err(StreamError::SpectrumGated { .. }) => inc(&sc.rows_gated),
            Err(_) => inc(&sc.errors),
        }
    }

    fn finish(mut self) {
        if let Some(p) = self.publisher.take() {
            p.finish();
        }
    }
}

fn stft_for(shared: &Shared, plan: &RowPlan) -> anyhow::Result<StftProcessor> {
    crate::compute::stft(
        &shared.compute,
        &shared.counters.compute,
        Reader::Spectrum,
        plan.stft,
    )
}

/// Replaces the STFT for `plan`, carrying its counters into `bases` (frames, resets). Rows still
/// in flight on an asynchronous provider belong to the old plan: they are published under it
/// first (T-056).
fn rebuild(
    shared: &Shared,
    plan: RowPlan,
    stft: &mut StftProcessor,
    out: &mut Output<'_>,
    bases: &mut (u64, u64),
) -> anyhow::Result<()> {
    stft.flush(|frame| out.row(frame));
    let st = stft.stats();
    bases.0 += st.frames;
    bases.1 += st.resets;
    *stft = stft_for(shared, &plan)?;
    out.set_plan(plan);
    Ok(())
}

/// Runs reader 3 until the ring closes.
pub(crate) fn run(
    shared: Arc<Shared>,
    attention: Option<Arc<AttentionService>>,
) -> anyhow::Result<()> {
    let class = shared.cfg.source_class;
    let display = Arc::clone(&shared.display);
    let mut seen = display.generation();
    let mut settings = display.get();
    let mut fs = shared.fs;
    let plan = row_plan(
        fs,
        settings.fft_size,
        settings.rows_per_s,
        class,
        settings.window,
    );
    let mut stft = stft_for(&shared, &plan)?;
    let mut out = Output::new(&shared, plan, settings, attention);
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let rc = &shared.counters.spectrum_reader;
    let mut bases = (0u64, 0u64);
    loop {
        let g = display.generation();
        if g != seen {
            seen = g;
            let next = display.get();
            let geometry = next.fft_size != settings.fft_size
                || next.rows_per_s != settings.rows_per_s
                || next.window != settings.window;
            settings = next;
            out.settings = next;
            if geometry {
                let plan = row_plan(
                    fs,
                    settings.fft_size,
                    settings.rows_per_s,
                    class,
                    settings.window,
                );
                rebuild(&shared, plan, &mut stft, &mut out, &mut bases)?;
            }
        }
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                let rate = chunk.provenance.tune.sample_rate_hz;
                if rate.is_finite() && rate > 0.0 && rate != fs {
                    fs = rate;
                    let plan = row_plan(
                        fs,
                        settings.fft_size,
                        settings.rows_per_s,
                        class,
                        settings.window,
                    );
                    rebuild(&shared, plan, &mut stft, &mut out, &mut bases)?;
                }
                if out.publisher.is_none() {
                    // Offer the stream as soon as samples arrive (clients connect before the
                    // first row), described by this chunk's window; a row whose own window
                    // differs still gets a new header first.
                    let key = HeaderKey {
                        center_hz: chunk.provenance.tune.center_hz,
                        span_hz: fs,
                        bins: out.plan.stft.welch.fft_len,
                        declared_hz: out.plan.declared_hz,
                    };
                    out.reoffer(key)?;
                }
                stft.push(InputInfo::from(&chunk), &buf[..chunk.len], |frame| {
                    out.row(frame)
                });
                if let Some(e) = out.error.take() {
                    return Err(e);
                }
                cursor.set(chunk.end_sample());
                add(&rc.samples, chunk.len as u64);
            }
            // Nothing new for a read timeout: publish an asynchronous provider's rows (T-056).
            ReadOutcome::Empty if stft.in_flight() > 0 => {
                stft.flush(|frame| out.row(frame));
                if let Some(e) = out.error.take() {
                    return Err(e);
                }
            }
            ReadOutcome::Overrun { .. } | ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
        set(&rc.lost_samples, reader.lost_samples());
        set(&rc.overruns, reader.overruns());
        set(&rc.gap_samples, reader.gap_samples());
        let st = stft.stats();
        set(&rc.frames, bases.0 + st.frames);
        set(&rc.stft_resets, bases.1 + st.resets);
    }
    // Stream end or detach: rows still in flight are published before the publisher finishes.
    stft.flush(|frame| out.row(frame));
    let st = stft.stats();
    set(&rc.frames, bases.0 + st.frames);
    set(&rc.stft_resets, bases.1 + st.resets);
    if let Some(e) = out.error.take() {
        return Err(e);
    }
    drop(cursor);
    out.finish();
    Ok(())
}
