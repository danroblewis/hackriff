//! SIGNAL-062 through the composed pipeline (T-027): `hk replay`'s path over the real HackRF FM
//! fixture (`fm_100p8M_2p4M_l32g30a1_t1p5_5s`: 2.4 Msps at 100.8 MHz, 101.3 MHz station, 5 s).
//!
//! The station is detected and tracked; its confirmed track selects the `wfm-rds` chain from the
//! registry by priors (FM broadcast band, 60–400 kHz, not bursty), which attaches at runtime,
//! auto-selects WFM, decodes RDS PI 1694 and labels the Emitter; a pre-trigger SigMF recording is
//! written (the FM band prior permits content); history tiles and the floor product are written;
//! spectrum stream records reach a consumer. Skips when the LFS data is not fetched.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
use hk_model::{
    AnnotationTarget, ContentClass, DecodedIdentity, FreqRange, IdentityScheme, InventoryIdentity,
    InventoryQuery, Region, TimeRange, Timestamp,
};
use hk_stream::{Declared, StreamKind};
use serde_json::json;

const SIGNAL_062: &str = "SIGNAL-062";
const NAME: &str = "fm_100p8M_2p4M_l32g30a1_t1p5_5s";

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

#[test]
fn signal_062_fm_station_detected_tracked_wfm_chain_rds_pi_label_tiles_and_stream() {
    let Some(meta) = real_fixture(NAME) else {
        return;
    };
    let dir = TempDir::new("signal062");
    let (mut cfg, replay) = replay_config(&dir.0, &meta, json!({}), hk_core::Pacing::Unpaced);
    assert_eq!(replay.class, ContentClass::Unrestricted, "FM band prior");
    let consumer = Buf::default();
    let sink_buf = consumer.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.kind == StreamKind::Spectrum {
            handle
                .subscribe(
                    "test-consumer",
                    Declared::local(sink_buf.clone()),
                    Box::new(|_| {}),
                )
                .unwrap();
        }
    }));
    let handle = start(cfg, replay);
    let product = handle.floor_product();
    let s = handle.wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.always_on_lost_samples, 0);

    // Detected and tracked.
    let repo = repo(&dir.0);
    let ever = TimeRange::new(
        Timestamp::from_unix_nanos(0),
        Timestamp::from_unix_nanos(i64::MAX / 2),
    );
    let station = FreqRange::centered(101.3e6, 150e3);
    let hits = repo
        .detections_in_region(&Region::new(station, ever))
        .unwrap();
    assert!(!hits.is_empty(), "[{SIGNAL_062}] 101.3 MHz not detected");
    assert!(s.counter("/detect/tracks_confirmed") >= 1);
    assert!(s.counter("/detect/track_rows") >= 1);

    // WFM chain auto-attached from the registry; RDS PI decoded; Emitter labelled.
    assert!(
        s.counter("/chains/attached") >= 1,
        "[{SIGNAL_062}] no chain"
    );
    assert!(s.counter("/chains/demodulations") >= 1);
    let identity = DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "1694".into(),
    };
    let decodes = repo.decodes_for_identity(&identity).unwrap();
    assert!(
        !decodes.is_empty(),
        "[{SIGNAL_062}] RDS PI 1694 not decoded"
    );
    assert!(
        decodes
            .iter()
            .all(|d| d.content_class == ContentClass::Unrestricted)
    );
    let entries = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::RdsPi),
            ..InventoryQuery::default()
        },
    );
    let emitter = entries
        .iter()
        .find(|e| {
            matches!(&e.identity, InventoryIdentity::Clear { identity: i, .. } if *i == identity)
        })
        .unwrap_or_else(|| panic!("[{SIGNAL_062}] no inventory emitter with PI 1694: {entries:?}"));
    let labels = repo
        .annotations_for(&AnnotationTarget::Emitter(emitter.emitter.id))
        .unwrap();
    assert!(!labels.is_empty(), "[{SIGNAL_062}] emitter label");
    eprintln!(
        "[{SIGNAL_062}] emitter {} label {:?}, {} decodes",
        emitter.emitter.id,
        labels[0].value,
        decodes.len()
    );
    assert!(s.counter("/chains/labels") >= 1);
    assert!(
        s.counter("/chains/recordings") >= 1,
        "[{SIGNAL_062}] pre-trigger recording"
    );

    // History tiles and the floor product.
    assert!(s.counter("/history/tiles_written") > 0);
    let p = product.lock().unwrap();
    let t0 = hits.iter().map(|d| d.time.start).min().unwrap();
    let fvt = p
        .floor_vs_time(
            station,
            t0.saturating_add_nanos(-1_000_000_000),
            t0.saturating_add_nanos(10_000_000_000),
            hk_store::Resolution::Level(0),
        )
        .unwrap();
    assert!(
        fvt.steps.iter().any(|st| !st.is_gap()),
        "[{SIGNAL_062}] floor product has no observed step"
    );

    // Spectrum stream records reached a consumer (unrestricted: nothing gated).
    assert!(s.counter("/spectrum/rows") > 10);
    assert_eq!(s.counter("/spectrum/rows_gated"), 0);
    let bytes = consumer.0.lock().unwrap().len();
    assert!(
        bytes > 10 * 4096,
        "[{SIGNAL_062}] consumer received {bytes} bytes"
    );
}
