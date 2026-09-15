//! SIGNAL-001 through the composed pipeline (T-027): the synthetic `adsb_squitter` scenario
//! (4 aircraft × 4 DF17 squitters, 1090 MHz, 2.4 Msps) replayed; the window covers the
//! `adsb-readsb` coverage chain's band, so the chain attaches at runtime, feeds the readsb plugin
//! (a subprocess: GPL stays behind the process boundary) and its decodes land through `Ingest` as
//! inventory emitters. Skipped when `readsb` or the `hk-plugin-readsb` wrapper is not available.
//!
//! T-037b: the 0.1 s scene is shorter than the ring (the coverage chain must still attach), and a
//! 10 s unpaced scene (about 46 MiB of ci8, over five times readsb's 8 MiB input queue) must
//! reach readsb without a single dropped record. Replays are blind (annotations stripped).

mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::{IdentityScheme, InventoryQuery};
use serde_json::json;

const SIGNAL_001: &str = "SIGNAL-001";

fn readsb_available() -> bool {
    if let Ok(path) = std::env::var("HK_READSB") {
        return std::path::Path::new(&path).is_file();
    }
    Command::new("readsb")
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn wrapper_built(cfg: &hk_pipeline::PipelineConfig) -> bool {
    let built = cfg
        .plugin_dirs
        .iter()
        .any(|d| d.join("hk-plugin-readsb").is_file());
    if !built {
        eprintln!(
            "SKIP {SIGNAL_001}: hk-plugin-readsb not built next to the test binary \
             (cargo build -p hk-plugins)"
        );
    }
    built
}

#[test]
fn signal_001_adsb_squitters_decoded_by_the_readsb_plugin_chain() {
    if !readsb_available() {
        eprintln!("SKIP {SIGNAL_001}: readsb not found on PATH");
        return;
    }
    let out = synth_or_skip!(
        SynthRequest::new("adsb_squitter")
            .seed(7)
            .param("duration_s", 0.1)
            .param("messages_per_aircraft", 4)
    );
    let fx = out.fixture(0).unwrap();
    let dir = TempDir::new("signal001");
    let (cfg, replay, _input) =
        blind_replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    if !wrapper_built(&cfg) {
        return;
    }
    assert_eq!(replay.class, hk_model::ContentClass::Unrestricted);
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(120));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(
        s.counter("/chains/attached"),
        1,
        "[{SIGNAL_001}] coverage chain"
    );
    assert_eq!(
        s.counter("/chains/plugin_decodes"),
        16,
        "[{SIGNAL_001}] 16 squitters decoded"
    );
    let repo = repo(&dir.0);
    let aircraft = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..InventoryQuery::default()
        },
    );
    assert_eq!(aircraft.len(), 4, "[{SIGNAL_001}] one emitter per ICAO");
    // T-039 family step for plugin decodes (T-037b): readsb → ADS-B is each top explanation.
    for e in &aircraft {
        let ranked = hk_pipeline::explanations(&repo, e.emitter.id).unwrap();
        assert_eq!(
            ranked.first().map(|x| x.service.as_str()),
            Some("adsb"),
            "[{SIGNAL_001}] {ranked:?}"
        );
    }
}

#[test]
fn signal_001_long_unpaced_replay_waits_for_readsb_instead_of_dropping() {
    if !readsb_available() {
        eprintln!("SKIP {SIGNAL_001}: readsb not found on PATH");
        return;
    }
    let out = synth_or_skip!(
        SynthRequest::new("adsb_squitter")
            .seed(11)
            .param("duration_s", 10.0)
            .param("messages_per_aircraft", 4)
    );
    let fx = out.fixture(0).unwrap();
    let truth = fx.of_kind("adsb-df17").len() as u64;
    let dir = TempDir::new("signal001-long");
    let (cfg, replay, _input) =
        blind_replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    if !wrapper_built(&cfg) {
        return;
    }
    let (s, fired) = wait_guarded(start(cfg, replay), Duration::from_secs(300));
    eprintln!("{}", s.to_text());
    assert!(!fired, "the run did not finish; the watchdog stopped it");
    assert!(s.errors.is_empty(), "{:?}", s.errors);
    assert_eq!(s.counter("/chains/attached"), 1);
    assert_eq!(
        s.counter("/chains/plugin_dropped"),
        0,
        "[{SIGNAL_001}] lossless replay drops no plugin input"
    );
    assert_eq!(
        s.counter("/chains/plugin_samples"),
        s.counter("/source/samples"),
        "[{SIGNAL_001}] every sample reached readsb"
    );
    assert_eq!(s.counter("/chains/plugin_wait_timeouts"), 0);
    // T-223: the chain holds its first record until the wrapper reports ready.
    assert_eq!(s.counter("/chains/plugin_fed_before_ready"), 0);
    assert_eq!(s.counter("/chains/plugin_ready_timeouts"), 0);
    assert_eq!(
        s.counter("/chains/plugin_decodes"),
        truth,
        "[{SIGNAL_001}] every squitter decoded"
    );
}
