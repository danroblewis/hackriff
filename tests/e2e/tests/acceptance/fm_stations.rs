//! SIGNAL-062 (T3, T-926): **RDS decodes on every captured station, unprompted** — the regression
//! guard for "tune to the FM band and, without clicking anything, most stations show their RDS name
//! within seconds".
//!
//! **The set, not one blob.** `fixtures/hackrf/explorer-2026-09-25/` (T-935, T-960) holds the
//! explorer agent's narrow FM captures (2.4 Msps, ~5 s, 24 MB each), clipped from the live app's IQ
//! ring and annotated with **hidden** `hackriff:truth`: each station's PI as the independent oracle
//! (`py/fixtures/rds_ref.py`, not the app's `hk-rds`) read it from the same IQ. The PS is not
//! asserted: every PS in the set is a dynamic song/artist scroll, not a station name. This test
//! runs every capture in the directory, so a station added there joins it.
//!
//! **Relation to "all captured signals decode" (T-936).** That set asserts per-station detection,
//! classification, explanation and decode on the same captures, served in the replay's default
//! blocks. This case is narrower and deliberately kept apart: it is the RC-tier guard for T-926's
//! window-length defect, the one case that serves the captures in HackRF-sized transfers.
//!
//! **Blind, through the mock SDR (T-047).** Each capture's truth is stripped and sealed before the
//! device sees it (`blind_replay`); nothing is selected, tuned, prompted or configured — the
//! built-in registry, detection, tracking and the `wfm-rds` chain decide everything. The truth is
//! read only here, after the run, and matched against the whole inventory by frequency extent.
//!
//! **What counts.** Every truth emission carrying an identity is an *RDS station*; each must reach
//! an inventory entry at its extent whose decoded identity is that PI. An emission with a pilot but no PI the reference decoder could
//! read (the weak 98.1 MHz station in the 98.5 MHz capture) is not an RDS station and is not
//! counted either way. The ratio — decoded / RDS stations — is printed, and must be 1.
//!
//! **Served as the air serves it.** The device delivers the captures in a live HackRF's
//! 65 536-sample transfers, not the replay's 5 ms blocks. On 2026-09-25 every full WFM window the
//! chain wrote on live air was 0.5 s or 1.0 s — the probe or the leading window — because a chunk
//! that crossed a collection stage's end was cut there and the next stage read the gap as a
//! discontinuity; 5 ms blocks divide every stage length, so a default replay decoded both stations
//! while live air decoded one of nineteen. With HackRF-sized transfers this test fails on that
//! defect (A4FF is lost).
//!
//! What it does **not** cover: the replay is lossless, so a chain is never lapped and no trigger
//! track closes under it before its window is read. The unit tests beside
//! `chains::analog::collect` pin those.

use std::path::{Path, PathBuf};

use hk_e2e::blind::matching;
use hk_e2e::{Fixture, TruthItem};
use hk_model::{IdentityScheme, InventoryEntry, InventoryIdentity, InventoryQuery};

use crate::blind::{BlindRun, BlindSource, blind_replay, center_tol_hz};
use crate::common::*;

const SIGNAL_062: &str = "SIGNAL-062";
/// The per-station capture set.
const SET_DIR: &str = "fixtures/hackrf/explorer-2026-09-25";
/// Samples per transfer the device serves the captures in: a live HackRF's (131 072 bytes of
/// ci8). The replay's default 5 ms blocks divide every stage length a chain collects (0.5 s is
/// 100 of them), and hid the T-926 defect that dropped the rest of a chunk at each stage boundary
/// and cut every live WFM window to 0.5 s.
const HACKRF_TRANSFER_SAMPLES: usize = 65_536;

/// Every capture of the set whose LFS data is fetched (a missing one skips, or fails under
/// `HK_REQUIRE_FIXTURES=1`, exactly as [`real_fixture_in`] decides).
fn captures() -> Vec<PathBuf> {
    let dir = hk_e2e::paths::repo_root().join(SET_DIR);
    let mut stems: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("[{SIGNAL_062}] {}: {e}", dir.display()))
        .filter_map(|e| {
            let p = e.ok()?.path();
            (p.extension()? == "sigmf-meta").then(|| p.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    stems.sort();
    stems
        .iter()
        .filter_map(|s| real_fixture_in(SET_DIR, s))
        .collect()
}

/// One station's outcome.
#[derive(Debug)]
struct Station {
    capture: String,
    center_hz: f64,
    truth_pi: String,
    decoded: Vec<String>,
}

/// The RDS PIs decoded on the inventory entries at `t`'s extent.
fn decoded_at(t: &TruthItem, all: &[InventoryEntry]) -> Vec<String> {
    matching(
        t,
        0.0,
        all,
        |e| (e.emitter.f_center_hz, e.emitter.bandwidth_hz),
        center_tol_hz(t),
    )
    .into_iter()
    .filter_map(|e| match &e.identity {
        InventoryIdentity::Clear { identity, .. } if identity.scheme == IdentityScheme::RdsPi => {
            Some(identity.value.to_ascii_uppercase())
        }
        _ => None,
    })
    .collect()
}

/// Runs one capture blind and scores its RDS stations.
fn run_capture(meta: &Path) -> Vec<Station> {
    let name = meta.file_stem().unwrap().to_string_lossy().into_owned();
    let fx = Fixture::load(meta).unwrap();
    let source = BlindSource {
        transfer_len: Some(HACKRF_TRANSFER_SAMPLES),
        ..BlindSource::default()
    };
    let BlindRun { dir, summary, .. } = blind_replay(meta, "fmsta", source);
    assert_eq!(
        summary.always_on_lost_samples, 0,
        "[{SIGNAL_062}] {name}: the lossless replay lost samples"
    );
    let repo = repo(&dir.0);
    let all = inventory(&repo, InventoryQuery::default());
    fx.emissions()
        .into_iter()
        .filter(|t| t.kind.starts_with("wfm-broadcast"))
        .filter_map(|t| {
            let (scheme, pi) = t.identity()?;
            assert_eq!(scheme, "rds_pi", "[{SIGNAL_062}] {name}: identity scheme");
            Some(Station {
                capture: name.clone(),
                center_hz: t.center_hz(),
                truth_pi: pi.to_ascii_uppercase(),
                decoded: decoded_at(t, &all),
            })
        })
        .collect()
}

#[test]
fn fm_stations_every_captured_rds_station_decodes_unprompted() {
    let metas = captures();
    if metas.is_empty() {
        return;
    }
    let stations: Vec<Station> = metas.iter().flat_map(|m| run_capture(m)).collect();
    assert!(
        !stations.is_empty(),
        "[{SIGNAL_062}] the set's truth names no RDS station"
    );
    let ok = |s: &Station| s.decoded.contains(&s.truth_pi);
    let decoded = stations.iter().filter(|s| ok(s)).count();
    eprintln!(
        "[{SIGNAL_062}] explorer FM set: {decoded} / {} RDS stations decoded unprompted over {} \
         captures",
        stations.len(),
        metas.len()
    );
    for s in &stations {
        eprintln!(
            "[{SIGNAL_062}]   {} {:.4} MHz: truth PI {}, decoded {:?}",
            s.capture,
            s.center_hz / 1e6,
            s.truth_pi,
            s.decoded
        );
    }
    let failed: Vec<&Station> = stations.iter().filter(|s| !ok(s)).collect();
    assert!(
        failed.is_empty(),
        "[{SIGNAL_062}] {} of {} captured RDS stations did not decode their PI blind: {failed:#?}",
        failed.len(),
        stations.len()
    );
}
