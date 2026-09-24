//! `SIGNAL-085` — automatic control-channel discovery **anywhere**, not only at 851–869 MHz (T-615).
//!
//! T-545 ruled that the built-in `trunk-cc-hunt` chain's trigger band is a band gate rather than a
//! frequency lookup, and recorded the gap that ruling leaves: SIGNAL-085 claims to find a 100 %-duty
//! narrowband four-level emission with no prior research, and what ran was gated to the 800 MHz
//! allocation (plus UHF, 700 and 900 — but **not** VHF, which the use case names first).
//!
//! T-615's decision, recorded on `Trigger::Occupancy` in
//! `crates/hk-pipeline/src/chains/spec.rs`: **the band is a dwell
//! budget, not a search prior.** Nothing inside the hunt reads it — the raster origin is where the
//! device says it is tuned, candidacy is measured occupancy against the window's own floor, and
//! confirmation is frame sync plus CRC. The band only decides whether a pass (one window held,
//! up to eight demodulations) is spent. So the claim splits into three measurements, one per test,
//! each asserted with counts over one run through the mock SDR and never with wall-clock:
//!
//! 1. [`a_vhf_control_channel_is_found_by_the_built_in_registry`] — the scene moved to VHF
//!    (155 MHz, outside the old `[851, 869]` trigger band and, before T-615, outside every band the
//!    registry gated on) is confirmed with the **built-in** registry and nothing configured.
//! 2. [`outside_every_lmr_band_the_built_in_hunt_does_not_spend_a_pass`] — the scene at 300 MHz,
//!    in no land-mobile allocation, with the built-in registry: the hunt does **not** run. This is
//!    the gate stated as a count rather than hidden, so the budget cannot silently widen or vanish.
//! 3. [`a_hunt_widened_to_the_whole_device_range_finds_it_at_300_mhz`] — the same 300 MHz scene
//!    with the hunt's band widened to 1 MHz–6 GHz by a plan, and no other change: the control
//!    channel is confirmed and the continuous decoy rejected. The capability is measurement-driven;
//!    only its budget is band-shaped.
//!
//! Blind throughout: the only frequency anything is given is the centre the mock device reports.
//! The plan in (3) names a range covering the whole device, which is a budget, not a location.
//! Truth is read only after each run, only to check the answer.

use hk_core::Pacing;
use hk_e2e::{Fixture, SynthRequest};
use hk_pipeline::RunSummary;
use serde_json::json;

use crate::blind::{replay_config, start};
use crate::common::*;

const S085: &str = "T-615/SIGNAL-085";

/// VHF high-band LMR (150.8–174 MHz), on a 12.5 kHz channel: outside `[851, 869]` MHz.
const VHF_CENTER_HZ: f64 = 155.0125e6;
/// In no land-mobile allocation at all (225–400 MHz is federal/military aeronautical).
const NON_LMR_CENTER_HZ: f64 = 300.0125e6;

/// One blind run of the T-267/T-287 control-channel scene at `center_hz`.
struct Run {
    _dir: TempDir,
    summary: RunSummary,
    found_hz: Vec<f64>,
    fx: Fixture,
}

impl Run {
    fn count(&self, path: &str) -> u64 {
        self.summary.counter(path)
    }

    /// `(control channel, continuous decoy, raster)` from the truth, opened only now.
    fn truth(&self) -> (f64, f64, f64) {
        let scenario = self.fx.scenario().unwrap();
        let t = &scenario.value["trunking"];
        (
            t["control_channel"]["rf_center_hz"].as_f64().unwrap(),
            t["continuous_decoy"]["rf_center_hz"].as_f64().unwrap(),
            t["raster_hz"].as_f64().unwrap(),
        )
    }

    fn report(&self, what: &str) {
        eprintln!(
            "[{S085}] {what}: {} pass(es), {} raster channels, {} candidates, {} demodulated, \
             {} confirmed, rows at {:?} MHz",
            self.count("/chains/cc_passes"),
            self.count("/chains/cc_channels"),
            self.count("/chains/cc_candidates"),
            self.count("/chains/cc_demods"),
            self.count("/chains/cc_confirmed"),
            self.found_hz.iter().map(|f| f / 1e6).collect::<Vec<_>>()
        );
    }

    /// The control channel, and only it, is on file — T-287's assertions, at a new frequency.
    fn assert_found(&self, what: &str) {
        self.report(what);
        assert!(
            self.count("/chains/cc_passes") >= 1,
            "[{S085}] {what}: the hunt never ran"
        );
        assert!(
            self.count("/chains/cc_candidates") >= 2,
            "[{S085}] {what}: the control channel and the continuous decoy must both reach \
             candidacy, or confirmation is not what decided: {}",
            self.count("/chains/cc_candidates")
        );
        assert!(
            self.count("/chains/cc_demods") <= 8,
            "[{S085}] {what}: admission (max_demods 8) did not bound the demodulations"
        );
        let (cc_hz, decoy_hz, raster_hz) = self.truth();
        assert_eq!(
            self.found_hz.len(),
            1,
            "[{S085}] {what}: exactly one control channel should be on file, got {:?} MHz \
             (truth {:.4} MHz)",
            self.found_hz.iter().map(|f| f / 1e6).collect::<Vec<_>>(),
            cc_hz / 1e6
        );
        let found = self.found_hz[0];
        assert!(
            (found - cc_hz).abs() <= raster_hz / 2.0,
            "[{S085}] {what}: the row says {:.4} MHz but the control channel is at {:.4} MHz",
            found / 1e6,
            cc_hz / 1e6
        );
        assert!(
            (found - decoy_hz).abs() > raster_hz / 2.0,
            "[{S085}] {what}: the continuous decoy at {:.4} MHz was confirmed",
            decoy_hz / 1e6
        );
        assert_eq!(self.count("/chains/cc_confirmed"), 1, "[{S085}] {what}");
        assert_eq!(self.count("/chains/cc_systems"), 1, "[{S085}] {what}");
    }
}

fn blind_run(tag: &str, center_hz: f64, extra: serde_json::Value) -> Option<Run> {
    let request = SynthRequest::new("trunk_control_channel")
        .seed(615)
        .param("duration_s", 2.0)
        .param("center_hz", center_hz);
    let out = match request.generate() {
        Ok(out) => out,
        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {}: {err}", module_path!());
            return None;
        }
        Err(err) => panic!("synthetic scenario generation failed: {err}"),
    };
    let fx = out.fixture(0).unwrap();
    let dir = TempDir::new(tag);
    let (cfg, dev) = replay_config(&dir.0, &fx.meta_path, extra, Pacing::Unpaced);
    let summary = finish(start(cfg, dev));
    let found_hz = repo(&dir.0)
        .trunk_systems()
        .unwrap()
        .iter()
        .map(|s| {
            s.cc_freq_hz
                .expect("a confirmed control channel always has a frequency")
        })
        .collect();
    Some(Run {
        _dir: dir,
        summary,
        found_hz,
        fx,
    })
}

/// `ScanPlan.extra` with the built-in registry, the hunt's band widened to the whole device.
fn hunt_everywhere() -> serde_json::Value {
    let mut chains = hk_pipeline::builtin_chains();
    let hunt = chains
        .iter_mut()
        .find(|c| c.id == "trunk-cc-hunt")
        .expect("the hunt is built in");
    hunt.freq_hz = vec![[1e6, 6e9]];
    json!({ "pipeline": { "chains": chains } })
}

#[test]
fn a_vhf_control_channel_is_found_by_the_built_in_registry() {
    let Some(run) = blind_run("s085vhf", VHF_CENTER_HZ, json!({})) else {
        return;
    };
    run.assert_found("VHF 155 MHz, built-in registry");
}

#[test]
fn outside_every_lmr_band_the_built_in_hunt_does_not_spend_a_pass() {
    let Some(run) = blind_run("s085gate", NON_LMR_CENTER_HZ, json!({})) else {
        return;
    };
    run.report("300 MHz, built-in registry");
    assert_eq!(
        run.count("/chains/cc_passes"),
        0,
        "[{S085}] the built-in hunt ran at 300 MHz: the dwell-budget gate recorded on \
         `Trigger::Occupancy` is gone, so either widen SIGNAL-085's claim to match or restore it"
    );
    assert!(
        run.found_hz.is_empty(),
        "[{S085}] a control channel was written with no hunt: {:?}",
        run.found_hz
    );
}

#[test]
fn a_hunt_widened_to_the_whole_device_range_finds_it_at_300_mhz() {
    let Some(run) = blind_run("s085any", NON_LMR_CENTER_HZ, hunt_everywhere()) else {
        return;
    };
    run.assert_found("300 MHz, hunt band 1 MHz-6 GHz");
}
