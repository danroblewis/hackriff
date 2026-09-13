//! SIGNAL-001 through the composed pipeline (T-027): the synthetic `adsb_squitter` scenario
//! (4 aircraft × 4 DF17 squitters, 1090 MHz, 2.4 Msps) replayed; the window covers the
//! `adsb-readsb` coverage chain's band, so the chain attaches at runtime, feeds the readsb plugin
//! (a subprocess: GPL stays behind the process boundary) and its decodes land through `Ingest` as
//! inventory emitters. Skipped when `readsb` or the `hk-plugin-readsb` wrapper is not available.

mod common;

use std::process::{Command, Stdio};

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
    let (cfg, replay) = replay_config(&dir.0, &fx.meta_path, json!({}), hk_core::Pacing::Unpaced);
    if !cfg
        .plugin_dirs
        .iter()
        .any(|d| d.join("hk-plugin-readsb").is_file())
    {
        eprintln!(
            "SKIP {SIGNAL_001}: hk-plugin-readsb not built next to the test binary \
             (cargo build -p hk-plugins)"
        );
        return;
    }
    assert_eq!(replay.class, hk_model::ContentClass::Unrestricted);
    let s = start(cfg, replay).wait().unwrap();
    eprintln!("{}", s.to_text());
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
}
