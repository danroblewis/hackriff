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
//!   on a centre, rate or gain change, so no row mixes two windows. While no row is being produced
//!   at all (T-489 below) there is nothing for the header to follow, so it follows the *chunk's*
//!   window instead and the offer stays true to what the front end is tuned to.
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
//! one. What can never be saved either way is the ring read and the `cursor.set` below: reader 3
//! holds a gate cursor, so it drains the ring whether or not anyone is looking, or a lossless run
//! stalls capture behind it.
//!
//! **So the FFT is skipped while nothing is subscribed (T-489).** A *watcher* is an open consumer
//! of the publisher in force — a `/ws/spectrum/live` websocket, a TCP stream client, an in-process
//! `subscribe` — and nothing else reads these rows ([`Output::watched`] lists why). With none, the
//! chunk is read, the gate cursor is advanced and the publisher is kept offered and current, but
//! `stft.push` is skipped; the saving is ~9% of pipeline CPU (measured below), now reachable by
//! the headless scheduled survey (workflow #2, hours on battery, no browser). **Nothing recorded
//! changes**: history, detection, the IQ ring and the receiver-line survey each hold their own
//! reader and their own STFT, so the tile pyramid, the coverage map, detections and every stored
//! product are byte-for-byte what they were — the only difference is display rows nobody asked
//! for, and `/spectrum/rows` honestly counting them at zero. Going idle flushes then clears the
//! STFT so its partial buffer cannot be stitched across the gap into a frame with a timestamp
//! from the far side; resuming restarts the average and flags the first row `DISCONTINUITY`.
//!
//! **Re-measured on today's main, and the 13% is nearer 9%.** Same subject and method as T-348
//! (`hk serve --replay` of the 2.4 Msps FM fixture, `--loop`, fft 4096, 25 rows/s, headless, three
//! interleaved 60 s windows of process CPU after a 15 s warm-up), baseline against this change:
//! **10.50 / 10.74 / 11.46 CPU-s per 60 s of wall clock before, 9.75 / 9.65 / 10.21 after** —
//! 1.03 CPU-s saved per 60 s on the means, **9.4% of the pipeline's CPU** (0.182 → 0.165 cores).
//! All three pairs separate and the ranges do not overlap, but the effect is smaller than T-348's
//! 1.7 CPU-s, which its own note says varied ±9% with other agents on the box; the direction and
//! the order of magnitude hold, the exact 13% does not. Same run, the reader's own counters:
//! `reader spectrum 179 388 000 samples, 0 frames, lost 0` — the ring drained in full, no FFT.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hk_core::{Discontinuity, ReadOutcome};
use hk_dsp::{InputInfo, PowerUnit, SpectrumFrame, StftProcessor};
use hk_model::ContentClass;
use hk_store::history::{FrameOrigin, source_key};
use hk_stream::{
    BinaryRecord, Publisher, PublisherConfig, PublisherHandle, RecordFlags, StreamError,
};
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
    /// The publisher's handle, kept only to ask how many consumers are open (T-489).
    handle: Option<PublisherHandle>,
    key: Option<HeaderKey>,
    db: Vec<f32>,
    bytes: Vec<u8>,
    avg: Vec<f32>,
    avg_rows: u32,
    /// Samples went by unwatched since the last published row, so the next one is not contiguous
    /// with the one before it and says so (T-489).
    gap: bool,
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
            handle: None,
            key: None,
            db: Vec::new(),
            bytes: Vec::new(),
            avg: Vec::new(),
            avg_rows: 0,
            gap: false,
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

    /// Is anything subscribed to the publisher in force (T-489)?
    ///
    /// **This is the whole definition of "watched".** A watcher is an open consumer of *this*
    /// publisher — a `/ws/spectrum/live` websocket, a `hk-api` TCP stream client, or an in-process
    /// [`hk_stream::PublisherHandle::subscribe`] (what a test's `stream_sink` opens). Nothing else
    /// reads these rows: history, detection, the IQ buffer and the receiver-line survey each hold
    /// their own ring reader and their own STFT ([`crate::run`] spawns four independent readers),
    /// so no stored product is a consumer of this stream. Note it is the *current* publisher: a
    /// re-offer (a retune, a display-geometry change) builds a new one and its consumers have to
    /// resubscribe, which is exactly the interval in which nobody is reading.
    fn watched(&self) -> bool {
        // **The view lattice is a consumer, and unlike every other one it is STORED (T-501).**
        //
        // The paragraph above was true when T-489 was written and T-501 falsifies it: since the
        // canvas's finest tier is sized to *this* plan's own bin and row
        // ([`crate::history::view_geometry`]), the rows this reader produces are folded into the
        // view pyramid by [`Output::row`] and become history. Skipping the FFT because no browser
        // is attached would therefore leave a permanent hole in the recorded finest tier for every
        // interval nobody watched — and the canvas cannot tell that hole from "the radio never
        // looked", which is the one thing grey is allowed to mean. A saving that changes what is
        // recorded is not a saving; T-489's own first rule says so.
        //
        // So T-489's skip survives exactly where its premise still holds: a run with no view
        // lattice attached (`view_queue: None`). With one attached the FFT runs, and what remains
        // gated on a subscriber is everything downstream of it — the publish is still offered to
        // nobody and costs nothing.
        self.view.is_some() || self.handle.as_ref().is_some_and(|h| h.open_consumers() > 0)
    }

    /// Samples went by with nothing subscribed: the next row is not contiguous with the last one
    /// published, so it is flagged `DISCONTINUITY` and the average restarts ([`Self::row`] does
    /// both off `reset`).
    fn skipped(&mut self) {
        self.gap = true;
    }

    /// Finishes the publisher in force and offers one whose header describes `key`.
    fn reoffer(&mut self, key: HeaderKey) -> Result<(), anyhow::Error> {
        if let Some(old) = self.publisher.take() {
            // The successor is offered a few lines below, under the same id: a consumer that
            // lands in between is between windows, not at the end of the stream (T-530).
            old.finish_between_windows();
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
        self.handle = Some(p.handle());
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
        let gap = std::mem::take(&mut self.gap);
        let reset = gap || frame.discontinuity.bits() & !Discontinuity::STREAM_START.bits() != 0;
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

    /// Ends this segment's publisher.
    ///
    /// **T-530: a segment's end is not the stream's end.** A re-plumb ([`crate::run`]) tears this
    /// reader down and builds another around the same still-open device, which offers a new
    /// publisher under the same id when the new segment's first samples arrive — measured at
    /// ~0.17 s after T-525. For that gap the run has *not* finished, so the publisher says a
    /// successor is expected and a client arriving in it is told "not now", not "never again".
    /// `continues` is the run's own record of which end this is: it is set by the re-plumb before
    /// the segment is stopped, and false when the source ended or the user stopped the run.
    /// **T-541: a device failure is also "a successor is expected".** `continues` is set by the
    /// re-plumb *and* — since T-541 — by the capture thread when a read fails on a run that will
    /// be recovered, with the grace it needs ([`crate::run::RECOVERY_SUCCESSOR_GRACE`]). Before
    /// that, a device error answered every `/ws/spectrum/live` handshake `410 Gone` for the whole
    /// recovery, which is the T-530 defect reached by the other door.
    fn finish(mut self) {
        if let Some(p) = self.publisher.take() {
            if self.shared.continues.load(Ordering::SeqCst) {
                p.finish_between_windows_for(Duration::from_millis(
                    self.shared.successor_grace_ms.load(Ordering::SeqCst),
                ));
            } else {
                p.finish();
            }
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
/// first (T-056), and so is the averaging in progress (T-915, [`finish`]).
fn rebuild(
    shared: &Shared,
    plan: RowPlan,
    stft: &mut StftProcessor,
    out: &mut Output<'_>,
    bases: &mut (u64, u64),
) -> anyhow::Result<()> {
    finish(stft, out);
    let st = stft.stats();
    bases.0 += st.frames;
    bases.1 += st.resets;
    *stft = stft_for(shared, &plan)?;
    out.set_plan(plan);
    Ok(())
}

/// Ends `stft`'s stream into `out`: rows in flight, then the averaging in progress as a partial
/// row when it holds T-139's minimum (T-915).
///
/// **Why the partial row.** A re-plumbing retune ends this segment's ring, and this reader's
/// stream with it; the samples since the last full row — up to one row period, and on average
/// half of one — were captured, are in the IQ ring, and are what the coverage map calls observed.
/// These frames are the view lattice's finest node (T-484), so discarding them left the departed
/// band's last row observed and never measured: the dotted `AWAITING` strip T-911 photographed at
/// the top of every band the radio left. The row carries its true `n_avg` and span, exactly as a
/// T-139 partial does.
///
/// Cost: at most one frame per segment end or plan change, on the reader's own thread — nothing
/// per arriving row (T-453).
fn finish(stft: &mut StftProcessor, out: &mut Output<'_>) {
    let min = crate::history::partial_min_segments(out.plan.stft.averages);
    stft.finish(min, |frame| out.row(frame));
}

/// Runs reader 3 until the ring closes.
///
/// `_view_producer` is this reader's claim on the view queue ([`crate::history::ViewProducer`],
/// T-915): held until this returns — on every path, errors and unwinding included — so the view
/// writer ends only after the last row this reader pushes.
pub(crate) fn run(
    shared: Arc<Shared>,
    attention: Option<Arc<AttentionService>>,
    _view_producer: Option<crate::history::ViewProducer>,
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
    // Was anything subscribed at the last chunk? Starts false: nothing can be subscribed before
    // the publisher has been offered, which the first chunk does.
    let mut was_watched = false;
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
                // This chunk's window, as the header would describe it. The publisher is offered
                // and kept current whether or not anything is subscribed: `reoffer` is what
                // registers the handle with the hk-api bridge, so gating the offer on a consumer
                // would mean no consumer could ever arrive (T-489).
                let key = HeaderKey {
                    center_hz: chunk.provenance.tune.center_hz,
                    span_hz: fs,
                    bins: out.plan.stft.welch.fft_len,
                    declared_hz: out.plan.declared_hz,
                };
                if out.publisher.is_none() {
                    // Offer the stream as soon as samples arrive (clients connect before the
                    // first row), described by this chunk's window; a row whose own window
                    // differs still gets a new header first.
                    out.reoffer(key)?;
                }
                let watched = out.watched();
                if watched {
                    // Resuming needs nothing done here: the STFT was cleared when the idle began,
                    // so no sample from before the gap survives to be stitched across it, and the
                    // flag `skipped` left behind restarts the average and marks the first row.
                    stft.push(InputInfo::from(&chunk), &buf[..chunk.len], |frame| {
                        out.row(frame)
                    });
                    if let Some(e) = out.error.take() {
                        return Err(e);
                    }
                } else {
                    if was_watched {
                        // Going idle: publish what is already framed, then clear the processor.
                        // Its partial buffer would otherwise survive the gap and be stitched into
                        // a frame carrying a timestamp from the far side of it.
                        stft.flush(|frame| out.row(frame));
                        if let Some(e) = out.error.take() {
                            return Err(e);
                        }
                        stft.reset();
                    }
                    out.skipped();
                    // No row will correct the header in force while none is produced, so the
                    // offer follows the window here instead (T-057's rule has no rows to follow).
                    if out.key != Some(key) {
                        out.reoffer(key)?;
                    }
                }
                was_watched = watched;
                // Read and gate-cursor advance are NOT optional: reader 3 holds a gate cursor, so
                // a lossless run stalls capture behind it if it stops draining the ring.
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
    // Stream end or detach: rows still in flight, and the averaging in progress (T-915), are
    // published before the publisher finishes.
    finish(&mut stft, &mut out);
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
