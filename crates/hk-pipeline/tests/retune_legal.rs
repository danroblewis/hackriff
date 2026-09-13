//! T-050 legal sentinel: a live run tuned to broadcast FM (unrestricted) retunes through the
//! control plane into the 930.5 MHz paging band (47 CFR 24.129, restricted-paging). A scripted
//! radio plays the same synthetic FSK sensor scene in both windows, so the only difference is the
//! window's class.
//!
//! - **Positive control (before the retune):** under FM the FSK chain stores decode content and
//!   a manual recording may start.
//! - **After the retune:** the run re-plumbed with `restricted-paging`; bursts are still detected
//!   and framed (metadata flows), but no Decode written after the retune keeps content or a
//!   decoded identity, a manual recording is refused, no Recording row lies in the paging band,
//!   the inventory (the API's gated query) shows no identity there, every stream fed by the
//!   paging window carries `restricted-paging` with header-only bits, and none of the scene's
//!   payload bytes or payload bits reach any of those streams.
//!
//! **Streams are attributed by provenance, not offer time (T-063).** The FM segment's chains
//! finish while the re-plumb runs (the old segment is processed to its end before the new one
//! starts), and the FSK chain offers its bits stream when it flushes, so an FM-window bits stream
//! can be offered after the retune was requested. Each stream is attributed to the window of the
//! blocks that fed it: a bits stream by its bursts' measured RF centre and their times on that
//! window's side of the retune boundary, the spectrum stream by its header centre. Anything
//! unattributable counts as the paging window (fail closed), and every stream offered once the
//! retune returned must be the paging window's (the FM segment's threads have all ended by then).

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Cursor, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::*;
use hk_core::{Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_e2e::SynthRequest;
use hk_model::{ContentClass, InventoryIdentity, InventoryQuery, Repository, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{
    ControlFailure, Pipeline, PipelineConfig, RunSummary, SourceInfo, TrackInventory,
    builtin_chains, replay_plan,
};
use hk_stream::{
    BinaryData, Declared, Record, RecordFlags, StreamHeader, StreamKind, StreamReader,
};
use num_complex::Complex;

const FM: f64 = 100.8e6;
const PAGING: f64 = 930.5e6;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Each payload as the bits stream carries it: one `u8` (0 or 1) per bit, MSB first.
fn bit_sentinels(payload_hex: &[String]) -> Vec<Vec<u8>> {
    payload_hex
        .iter()
        .map(|h| {
            (0..h.len() / 2)
                .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
                .flat_map(|byte| (0..8).rev().map(move |k| (byte >> k) & 1))
                .collect()
        })
        .collect()
}

/// When a stream was offered, relative to the retune request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Offered {
    /// Before `retune` was called.
    BeforeRetune,
    /// While the re-plumb ran (the FM segment finishing, or the new segment starting).
    DuringReplumb,
    /// After `retune` returned.
    AfterRetune,
}

/// One stream a consumer received, with the window of the blocks that fed it.
struct Stream {
    header: StreamHeader,
    bytes: Vec<u8>,
    records: Vec<BinaryData>,
    offered: Offered,
    /// Tune centre of the blocks that fed the stream (`None`: unattributable).
    window_hz: Option<f64>,
}

impl Stream {
    fn in_window(&self, center: f64, fs: f64) -> bool {
        self.window_hz
            .is_some_and(|w| (w - center).abs() <= 0.5 * fs)
    }

    /// Fed by the paging window; unattributable streams count as paging (fail closed).
    fn paging(&self, fs: f64) -> bool {
        self.window_hz.is_none() || self.in_window(PAGING, fs)
    }
}

/// The window a stream's blocks were captured under (`None`: unattributable). The spectrum
/// stream's header names its window's centre. A bits stream's header centre is its bursts'
/// measured RF centre, which lies inside the window that captured them; it counts only when the
/// bursts' times (every record's, each timed at its burst's first sample, and the stored
/// Bitstream descriptor's range when framing found one) lie on that window's side of the retune
/// boundary.
fn window_of(
    repo: &Repository,
    h: &StreamHeader,
    records: &[BinaryData],
    fs: f64,
    t_retune: i64,
) -> Option<f64> {
    let center = h.center_hz?;
    match h.kind {
        StreamKind::Spectrum => Some(center),
        StreamKind::Bits => {
            let mut times: Vec<(i64, i64)> = records
                .iter()
                .map(|r| (r.header.t.as_unix_nanos(), r.header.t.as_unix_nanos()))
                .collect();
            if let Some(id) = h.bitstream_id {
                let t = repo.bitstream(id).ok()?.time;
                times.push((t.start.as_unix_nanos(), t.end.as_unix_nanos()));
            }
            if times.is_empty() {
                return None;
            }
            let inside = |w: f64| (center - w).abs() <= 0.5 * fs;
            let fm = inside(FM) && times.iter().all(|t| t.1 <= t_retune);
            let paging = inside(PAGING) && times.iter().all(|t| t.0 >= t_retune);
            (fm || paging).then_some(center)
        }
        _ => None,
    }
}

struct Scene {
    dir: TempDir,
    summary: RunSummary,
    fs: f64,
    payloads: Vec<String>,
    decodes_fm: u64,
    withheld_fm: u64,
    t_retune: i64,
    streams: Vec<Stream>,
}

static SCENE: OnceLock<Option<Scene>> = OnceLock::new();

/// The FM → paging scene, run once for every test in this file (`None`: synthesis unavailable).
fn scene() -> Option<&'static Scene> {
    SCENE.get_or_init(run_scene).as_ref()
}

fn run_scene() -> Option<Scene> {
    let request = SynthRequest::new("fsk_burst_train")
        .seed(36)
        .param("snr_db", 20.0)
        .param("duration_s", 1.2);
    let out = match request.generate() {
        Ok(out) => out,
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {e}", module_path!());
            return None;
        }
        Err(e) => panic!("synthetic scenario generation failed: {e}"),
    };
    let fx = out.fixture(0).unwrap();
    let payloads: Vec<String> = fx
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    assert!(!payloads.is_empty());
    let fs = fx.sample_rate;
    assert_eq!(window_class(FM, fs), ContentClass::Unrestricted);
    assert_eq!(window_class(PAGING, fs), ContentClass::RestrictedPaging);

    // The scene's IQ, quantised as the capture thread does.
    let mut src = SigmfReplaySource::open(
        &fx.meta_path,
        ReplayOptions {
            block_len: 65_536,
            pacing: Pacing::Unpaced,
        },
    )
    .unwrap();
    let mut iq: Vec<Complex<i8>> = Vec::new();
    while let Some(b) = src.next_block().unwrap() {
        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
        iq.extend(b.samples.iter().map(|z| Complex::new(q(z.re), q(z.im))));
    }
    let pass = iq.len() as u64;

    let dir = TempDir::new("t050-retune-legal");
    let (radio, ctl) = radio::Radio::new(FM, fs, 16_384, radio::looped(iq));
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(FM, fs, t0)).unwrap();
    cfg.source_class = window_class(FM, fs);
    cfg.live_window_class = true;
    cfg.lossless = true;
    // Only the FSK chain: the FM band's WFM spec would otherwise claim the sensor's tracks.
    cfg.settings.chains = Some(
        builtin_chains()
            .into_iter()
            .filter(|c| c.id == "fsk-bursts")
            .collect(),
    );
    let offered: Arc<Mutex<Vec<(StreamHeader, Buf)>>> = Arc::default();
    let seen = Arc::clone(&offered);
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        let buf = Buf::default();
        let r = handle.subscribe(
            "t050-sentinel",
            Declared::local(buf.clone()),
            Box::new(|_| {}),
        );
        assert!(r.is_ok(), "subscribe to {}", h.stream_id);
        seen.lock().unwrap().push((h.clone(), buf));
    }));
    ctl.hold_at(pass);
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: fs,
            center_hz: FM,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let plane = handle.controller();
    let counters = handle.counters();
    let detect_read = || counters.detect_reader.samples.load(Ordering::Relaxed);

    // Phase 1: one pass of the scene under FM.
    assert!(ctl.wait_emitted(pass, Duration::from_secs(600)));
    wait("the FM pass to be read", Duration::from_secs(600), || {
        detect_read() >= pass
    });
    plane
        .start_recording(Some("fm"), Some(1.0))
        .expect("a manual recording may start under broadcast FM");
    plane.stop_recording().unwrap();
    let streams_before = offered.lock().unwrap().len();
    let t_retune = counters.stream_time_ns.load(Ordering::Relaxed);

    let outcome = plane.retune(PAGING, fs).expect("retune into paging");
    let streams_at_return = offered.lock().unwrap().len();
    assert!(outcome.replumbed, "another class: re-plumbed");
    assert_eq!(outcome.content_class, ContentClass::RestrictedPaging);
    assert_eq!(plane.status().content_class, ContentClass::RestrictedPaging);
    // The FM segment has finished: its chains flushed their decodes under the FM class.
    let decodes_fm = counters.chains.decodes.load(Ordering::Relaxed);
    let withheld_fm = counters.chains.content_withheld.load(Ordering::Relaxed);
    assert!(matches!(
        plane.start_recording(None, None),
        Err(ControlFailure::Refused(_))
    ));

    // Phase 2: the same scene in the paging window.
    ctl.hold_at(2 * pass);
    assert!(ctl.wait_emitted(2 * pass, Duration::from_secs(600)));
    wait(
        "the paging pass to be read",
        Duration::from_secs(600),
        || detect_read() >= 2 * pass,
    );
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(600));
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    // Every stream, parsed as a consumer reads it and attributed to its window.
    let repo = repo(&dir.0);
    let streams: Vec<Stream> = offered
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, (header, buf))| {
            let bytes = buf.0.lock().unwrap().clone();
            let mut reader = StreamReader::new(Cursor::new(bytes.clone()));
            assert_eq!(
                reader.read_header().unwrap(),
                header,
                "{}: the consumer received the offered header",
                header.stream_id
            );
            let mut records = Vec::new();
            while let Some(r) = reader.next_record().unwrap() {
                if let Record::Binary(b) = r {
                    records.push(b);
                }
            }
            let offered = if i < streams_before {
                Offered::BeforeRetune
            } else if i < streams_at_return {
                Offered::DuringReplumb
            } else {
                Offered::AfterRetune
            };
            Stream {
                window_hz: window_of(&repo, header, &records, fs, t_retune),
                header: header.clone(),
                bytes,
                records,
                offered,
            }
        })
        .collect();
    drop(repo);
    // Evidence: which segment's blocks fed each stream.
    for s in &streams {
        let (gated, payload) = s.records.iter().fold((0, 0), |(g, p), r| {
            (
                g + usize::from(r.header.flags.contains(RecordFlags::GATED)),
                p + r.payload.len(),
            )
        });
        let dt_ms = |r: &BinaryData| (r.header.t.as_unix_nanos() - t_retune) as f64 / 1e6;
        eprintln!(
            "stream {} ({:?}) {:?}: class {:?}, window {:?} Hz, header centre {:?} Hz, \
             bitstream {:?}, {} records ({gated} gated, {payload} payload bytes), \
             t - t_retune {:?}..{:?} ms",
            s.header.stream_id,
            s.header.kind,
            s.offered,
            s.header.content_class,
            s.window_hz,
            s.header.center_hz,
            s.header.bitstream_id,
            s.records.len(),
            s.records.first().map(dt_ms),
            s.records.last().map(dt_ms),
        );
    }
    Some(Scene {
        dir,
        summary,
        fs,
        payloads,
        decodes_fm,
        withheld_fm,
        t_retune,
        streams,
    })
}

#[test]
fn tuning_from_fm_into_930_5_mhz_paging_keeps_content_and_identity_out_after_the_retune() {
    let Some(sc) = scene() else { return };
    let (s, fs, t_retune) = (&sc.summary, sc.fs, sc.t_retune);
    let (decodes_fm, withheld_fm) = (sc.decodes_fm, sc.withheld_fm);
    assert!(
        decodes_fm > withheld_fm,
        "positive control: FM-window decodes keep content ({decodes_fm} decodes, {withheld_fm} withheld)"
    );
    assert_eq!(s.source_class, "restricted-paging");
    let decodes_paging = s.counter("/chains/decodes") - decodes_fm;
    let withheld_paging = s.counter("/chains/content_withheld") - withheld_fm;
    assert!(
        decodes_paging > 0,
        "bursts were framed after the retune (there was content to withhold)"
    );
    assert_eq!(
        withheld_paging, decodes_paging,
        "no decode after the retune keeps content"
    );

    // Database: nothing after the retune holds content or a decoded identity.
    let db = rusqlite::Connection::open(sc.dir.0.join("hackriff.db")).unwrap();
    let count = |sql: &str| -> i64 { db.query_row(sql, [t_retune], |r| r.get(0)).unwrap() };
    assert!(
        count("SELECT count(*) FROM decode WHERE t < ?1 AND has_content = 1") > 0,
        "positive control: content stored for the FM window"
    );
    assert!(count("SELECT count(*) FROM decode WHERE t >= ?1") > 0);
    assert_eq!(
        count("SELECT count(*) FROM decode WHERE t >= ?1 AND has_content = 1"),
        0,
        "content stored after the retune"
    );
    assert_eq!(
        count("SELECT count(*) FROM decode WHERE t >= ?1 AND content_class <> 'restricted-paging'"),
        0,
        "every decode after the retune carries the paging class"
    );
    // Identities: the repository keeps a decode's identity for entity resolution and gates it on
    // every read by the row's class (T-034/T-036, unchanged here). After the retune no identity
    // may sit under a class that opens it, and the gated read (what the API serves) shows none.
    assert_eq!(
        count(
            "SELECT count(*) FROM decode WHERE t >= ?1 AND identity_value IS NOT NULL \
             AND content_class IN ('unrestricted', 'own-key-decrypted')"
        ),
        0,
        "an identity stored under an open class after the retune"
    );
    let gated = repo(&sc.dir.0);
    let mut stmt = db.prepare("SELECT body FROM decode WHERE t >= ?1").unwrap();
    let ids: Vec<hk_model::DecodeId> = stmt
        .query_map([t_retune], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|b| {
            serde_json::from_str::<hk_model::Decode>(&b.unwrap())
                .unwrap()
                .id
        })
        .collect();
    assert!(!ids.is_empty());
    for id in ids {
        let d = gated.decode(id).unwrap();
        assert!(d.identity.is_none(), "identity served for decode {id}");
        assert!(d.content.is_none(), "content served for decode {id}");
        assert_eq!(d.content_class, ContentClass::RestrictedPaging);
    }
    drop(gated);
    assert_eq!(
        count(
            "SELECT count(*) FROM recording WHERE t_end >= ?1 AND f_center BETWEEN 929e6 AND 932e6"
        ),
        0,
        "no recording in the paging band"
    );

    // API view (gated inventory query): no identity in the paging band.
    let repo = repo(&sc.dir.0);
    let paging: Vec<_> = inventory(&repo, InventoryQuery::default())
        .into_iter()
        .filter(|e| (929e6..932e6).contains(&e.emitter.f_center_hz))
        .collect();
    assert!(
        !paging.is_empty(),
        "the sensor is in the inventory at 930.5 MHz"
    );
    for e in &paging {
        assert!(
            !matches!(e.identity, InventoryIdentity::Clear { .. }),
            "identity shown for {:?}",
            e.emitter.id
        );
    }

    // Streams, by the window that fed them: every paging-window stream is restricted-paging with
    // header-only bits, and no payload byte or payload bit reaches any of them.
    assert!(
        sc.streams
            .iter()
            .any(|st| st.offered != Offered::BeforeRetune
                && st.header.stream_id == "spectrum/live"
                && st.header.center_hz == Some(PAGING)),
        "the spectrum stream was offered again for the paging window"
    );
    let mut sent = sentinels(&sc.payloads);
    sent.extend(bit_sentinels(&sc.payloads));
    let mut hay = Vec::new();
    for st in &sc.streams {
        let id = &st.header.stream_id;
        if st.offered == Offered::AfterRetune {
            assert!(
                st.paging(fs),
                "{id} offered after the retune returned is fed by window {:?}",
                st.window_hz
            );
        }
        if !st.paging(fs) {
            assert!(
                st.in_window(FM, fs),
                "{id}: fed by neither window ({:?})",
                st.window_hz
            );
            continue;
        }
        assert_eq!(
            st.header.content_class,
            ContentClass::RestrictedPaging,
            "{id} fed by the paging window"
        );
        if st.header.kind == StreamKind::Bits {
            for r in &st.records {
                assert!(
                    r.header.flags.contains(RecordFlags::GATED) && r.payload.is_empty(),
                    "{id}: a paging-window bits record carries its payload"
                );
            }
        }
        hay.extend_from_slice(&st.bytes);
    }
    assert!(
        !hay.is_empty(),
        "the consumers received the paging-window streams"
    );
    assert_eq!(
        count_found(&hay, &sent),
        0,
        "payload bytes or bits in a paging-window stream"
    );
    assert!(
        s.counter("/spectrum/rows") > 0,
        "the gated spectrum stream kept flowing"
    );
}

/// T-063 regression, at the stream level: a bits stream created under the unrestricted FM segment
/// (even one offered while the re-plumb runs) carries only bursts from FM-window blocks, never
/// bits from blocks of the later restricted-paging segment; the paging segment's bits streams are
/// its own, restricted-paging and header-only.
#[test]
fn a_bits_stream_opened_under_fm_never_carries_bits_from_the_later_paging_segment() {
    let Some(sc) = scene() else { return };
    let (fs, t_retune) = (sc.fs, sc.t_retune);
    let bits: Vec<&Stream> = sc
        .streams
        .iter()
        .filter(|s| s.header.kind == StreamKind::Bits)
        .collect();
    let (fm, paging): (Vec<&Stream>, Vec<&Stream>) = bits.iter().partition(|s| s.in_window(FM, fs));
    assert!(!fm.is_empty(), "positive control: an FM-window bits stream");
    assert!(!paging.is_empty(), "a paging-window bits stream");

    let bit_sent = bit_sentinels(&sc.payloads);
    let mut fm_hay = Vec::new();
    for s in &fm {
        let id = &s.header.stream_id;
        assert_eq!(s.header.content_class, ContentClass::Unrestricted, "{id}");
        assert_ne!(
            s.offered,
            Offered::AfterRetune,
            "{id}: an FM-window stream offered after the paging segment started"
        );
        assert!(!s.records.is_empty(), "{id}: records");
        for r in &s.records {
            assert!(
                r.header.t.as_unix_nanos() <= t_retune,
                "{id}: a record from a block after the retune boundary ({} ns > {t_retune} ns)",
                r.header.t.as_unix_nanos()
            );
            assert!(
                !r.header.flags.contains(RecordFlags::GATED),
                "{id}: open class"
            );
            fm_hay.extend_from_slice(&r.payload);
        }
    }
    // The bit sentinels find content where it legitimately flows, so their absence below means
    // something.
    assert!(
        count_found(&fm_hay, &bit_sent) > 0,
        "positive control: FM-window bits streams carry the payload bits"
    );

    for s in &paging {
        let id = &s.header.stream_id;
        assert_eq!(
            s.header.content_class,
            ContentClass::RestrictedPaging,
            "{id}"
        );
        assert_ne!(s.offered, Offered::BeforeRetune, "{id}");
        for r in &s.records {
            assert!(
                r.header.t.as_unix_nanos() >= t_retune,
                "{id}: a record from a block before the retune boundary"
            );
            assert!(
                r.header.flags.contains(RecordFlags::GATED) && r.payload.is_empty(),
                "{id}: header-only"
            );
        }
        assert_eq!(count_found(&s.bytes, &bit_sent), 0, "{id}: payload bits");
    }
}
