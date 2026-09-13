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
//!   the inventory (the API's gated query) shows no identity there, every stream offered after the
//!   retune carries `restricted-paging`, and none of the scene's payload bytes reach any of those
//!   streams.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_core::{Pacing, ReplayOptions, SigmfReplaySource, Source};
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{ContentClass, InventoryIdentity, InventoryQuery, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{
    ControlFailure, Pipeline, PipelineConfig, SourceInfo, TrackInventory, builtin_chains,
    replay_plan,
};
use hk_stream::{Declared, StreamHeader};
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

#[test]
fn tuning_from_fm_into_930_5_mhz_paging_keeps_content_and_identity_out_after_the_retune() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
    );
    let fx = out.fixture(0).unwrap();
    let payloads: Vec<String> = fx
        .of_kind("fsk-burst")
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    assert!(!payloads.is_empty());
    let sent = sentinels(&payloads);
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
    let streams: Arc<Mutex<Vec<(StreamHeader, Buf)>>> = Arc::default();
    let seen = Arc::clone(&streams);
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
    let streams_before = streams.lock().unwrap().len();
    let t_retune = counters.stream_time_ns.load(Ordering::Relaxed);

    let outcome = plane.retune(PAGING, fs).expect("retune into paging");
    assert!(outcome.replumbed, "another class: re-plumbed");
    assert_eq!(outcome.content_class, ContentClass::RestrictedPaging);
    assert_eq!(plane.status().content_class, ContentClass::RestrictedPaging);
    // The FM segment has finished: its chains flushed their decodes under the FM class.
    let decodes_fm = counters.chains.decodes.load(Ordering::Relaxed);
    let withheld_fm = counters.chains.content_withheld.load(Ordering::Relaxed);
    assert!(
        decodes_fm > withheld_fm,
        "positive control: FM-window decodes keep content ({decodes_fm} decodes, {withheld_fm} withheld)"
    );
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
    let (s, fired) = wait_guarded(handle, Duration::from_secs(600));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
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
    let db = rusqlite::Connection::open(dir.0.join("hackriff.db")).unwrap();
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
    let gated = repo(&dir.0);
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
    let repo = repo(&dir.0);
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

    // Streams offered after the retune: all restricted-paging, and none carries payload bytes.
    let streams = streams.lock().unwrap().clone();
    let after = &streams[streams_before..];
    assert!(
        after
            .iter()
            .any(|(h, _)| h.stream_id == "spectrum/live" && h.center_hz == Some(PAGING)),
        "the spectrum stream was offered again for the paging window"
    );
    let mut hay = Vec::new();
    for (h, buf) in after {
        assert_eq!(
            h.content_class,
            ContentClass::RestrictedPaging,
            "{} offered after the retune",
            h.stream_id
        );
        hay.extend_from_slice(&buf.0.lock().unwrap());
    }
    assert!(
        !hay.is_empty(),
        "the consumers received the post-retune streams"
    );
    assert_eq!(
        count_found(&hay, &sent),
        0,
        "payload bytes in a post-retune stream"
    );
    assert!(
        s.counter("/spectrum/rows") > 0,
        "the gated spectrum stream kept flowing"
    );
}
