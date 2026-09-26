//! T-945 (SIGNAL-062): **automatic front-end gain management through the device interface**, with
//! the scene the explorer hit on 2026-09-25 (docs/28 §1): a front-end-limited dense FM band where
//! the front end is flagged `overload: true` and nothing decodes, and where the obvious "fix" —
//! turn the gain down — walks off the working point instead of onto it.
//!
//! The scene is three synthetic FM stations summed and quantised as a HackRF would: two strong ones
//! at −12 dBFS placed so a third-order product of the pair lands on the target, and the RDS target
//! at −34 dBFS. The mock SDR serves it behind the real device contract, applying its calibrated gain
//! model — digital scaling of the recorded IQ, receiver noise below the recording's gain, rounding
//! and **saturation at the 8-bit ADC**, which is what generates the intermodulation. Gain is the only
//! thing that moves: same centre, same rate, one open source throughout.
//!
//! What this asserts:
//!
//! - **converges**: from the overloaded state (LNA 32 / VGA 30 / amp on, 73 dB, ~86 % of components
//!   pinned, no decode) the loop ends on a state whose **CRC-valid RDS group rate** is strictly
//!   better, and recovers the target's PI — which the scene keeps as its private truth;
//! - **through the gated path**: every probe is one `DeviceAction::Gains` through the one
//!   `DeviceGate`, one action per probe plus the commit, and nothing else reaches the device;
//! - **bounded and non-oscillating**: no state is probed twice and the probe budget holds;
//! - **provenance per interval**: each probe's dwell carries its own provenance naming *its* gains,
//!   and a dwell that spans a discontinuity is not scored;
//! - **today's behaviour, for contrast**: with the policy off — the default, and what the explorer
//!   ran — no device action is issued at all, the radio stays where it was and nothing decodes.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use hk_api::gain::GainManager;
use hk_api::{LiveControl, LiveTuning, SourceLiveControl};
use hk_core::gain::{DecodeQuality, GainPolicy, GainProbe, GainQuality, GainState, GainTrigger};
use hk_core::{
    Discontinuity, Gains, MockOptions, MockSdrDriver, NamedGain, Pacing, ProvenanceHandle, Source,
    SourceControl,
};
use hk_demod::AnalogReceiver;
use hk_e2e::SynthRequest;
use hk_estimate::SnippetRequest;
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{BiasTee, SampleTime, Timestamp};
use num_complex::Complex;

const FS: f64 = 2e6;
const CENTER: f64 = 98.1e6;
/// The channel being worked: the target station sits at the capture centre.
const CHANNEL_BW: f64 = 200e3;
/// The scene's private truth, compared only after the run.
const TRUTH_PI: &str = "C0DE";
/// The state the explorer's radio was in: LNA 32 / VGA 30 / amp on.
const OVERLOADED: Gains = Gains {
    lna_db: 32.0,
    vga_db: 30.0,
    amp_on: true,
};

struct Station {
    offset_hz: f64,
    power_dbfs: f64,
    pi: &'static str,
    seed: u64,
}

fn read_ci8_at(path: &Path) -> Vec<Complex<i8>> {
    std::fs::read(path)
        .expect("read .sigmf-data")
        .chunks_exact(2)
        .map(|c| Complex::new(c[0] as i8, c[1] as i8))
        .collect()
}

fn station(s: &Station, secs: f64) -> Option<Vec<Complex<i8>>> {
    let req = SynthRequest::new("fm_broadcast_rds")
        .seed(s.seed)
        .param("sample_rate", FS)
        .param("center_hz", CENTER)
        .param("offset_hz", s.offset_hz)
        .param("duration_s", secs)
        .param("pi_hex", s.pi)
        .param("power_dbfs", s.power_dbfs)
        .param("noise_dbfs", -66.0);
    match req.generate() {
        Ok(out) => Some(read_ci8_at(&out.fixture(0).unwrap().data_path())),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP: synth unavailable: {e}");
            None
        }
        Err(e) => panic!("synth failed: {e}"),
    }
}

/// The dense-FM scene as a ci8 SigMF recording with **no** `hackriff:provenance`, so the mock takes
/// its nominal working gain (LNA 24 / VGA 20, 44 dB) as the gain reference.
fn scene(dir: &Path, secs: f64) -> Option<PathBuf> {
    // Two strong neighbours at −350 and −700 kHz: 2·(−350) − (−700) = 0, so the pair's third-order
    // product lands on the target at the capture centre once the ADC starts limiting.
    let stations = [
        Station {
            offset_hz: 0.0,
            power_dbfs: -34.0,
            pi: TRUTH_PI,
            seed: 945,
        },
        Station {
            offset_hz: -350e3,
            power_dbfs: -12.0,
            pi: "1A2B",
            seed: 946,
        },
        Station {
            offset_hz: -700e3,
            power_dbfs: -12.0,
            pi: "3C4D",
            seed: 947,
        },
    ];
    std::fs::create_dir_all(dir).unwrap();
    let mut sum: Vec<(i32, i32)> = Vec::new();
    for s in &stations {
        let iq = station(s, secs)?;
        if sum.is_empty() {
            sum = vec![(0, 0); iq.len()];
        }
        for (a, z) in sum.iter_mut().zip(&iq) {
            a.0 += i32::from(z.re);
            a.1 += i32::from(z.im);
        }
    }
    let mut data = Vec::with_capacity(sum.len() * 2);
    let mut peak = 0i32;
    for &(i, q) in &sum {
        peak = peak.max(i.abs()).max(q.abs());
        data.push(i.clamp(-128, 127) as i8 as u8);
        data.push(q.clamp(-128, 127) as i8 as u8);
    }
    assert!(
        peak < 127,
        "the recording itself must not clip, or the scene's overload is not the gain's doing \
         (peak {peak})"
    );
    std::fs::write(dir.join("fmband.sigmf-data"), data).unwrap();
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(FS);
    meta.captures.push(Capture {
        sample_start: 0,
        frequency: Some(CENTER),
        datetime: Some("2026-09-25T03:43:00Z".into()),
        provenance: None,
        clip_count: None,
        extra: serde_json::Map::new(),
    });
    let path = dir.join("fmband.sigmf-meta");
    meta.write(&path).unwrap();
    Some(path)
}

fn info<'a>(prov: &'a ProvenanceHandle) -> hk_dsp::InputInfo<'a> {
    hk_dsp::InputInfo {
        time: SampleTime {
            sample_index: 0,
            host_time: Timestamp::from_unix_nanos(1_789_000_000_000_000_000),
        },
        discontinuity: Discontinuity::STREAM_START,
        dropped_before: 0,
        provenance: prov,
    }
}

/// One dwell: contiguous samples taken **under the state the device took**, with the provenance they
/// were served with. `Err` rather than a guess when the stream cannot give one.
struct Dwell {
    iq: Vec<Complex<i8>>,
    prov: ProvenanceHandle,
}

fn collect(source: &mut dyn Source, want: usize, state: &GainState) -> Result<Dwell, String> {
    let mut iq: Vec<Complex<i8>> = Vec::with_capacity(want);
    let mut buf: Vec<Complex<i8>> = Vec::new();
    let mut prov: Option<ProvenanceHandle> = None;
    let mut read = 0usize;
    while iq.len() < want {
        let Some(h) = source
            .read_block_ci8(&mut buf)
            .map_err(|e| format!("the source stopped: {e}"))?
        else {
            return Err("the source ended mid-dwell".into());
        };
        read += buf.len();
        if read > 8 * want {
            return Err(format!(
                "no contiguous dwell of {want} samples at {state} after {read} samples"
            ));
        }
        let p = h.provenance.get();
        let matches = state.get("lna") == Some(p.tune.lna_db)
            && state.get("vga") == Some(p.tune.vga_db)
            && state.get("amp").map(|db| db > 0.0) == Some(p.tune.amp_on);
        // The settle gap: samples served before the change took effect are not this state's, and a
        // block that spans a discontinuity (the mock splicing the recording again) is not evidence.
        if !matches || h.discontinuity.contains(Discontinuity::GAP) {
            iq.clear();
            prov = None;
            continue;
        }
        prov = Some(h.provenance.clone());
        iq.extend_from_slice(&buf);
    }
    iq.truncate(want);
    Ok(Dwell {
        iq,
        prov: prov.expect("a matching block"),
    })
}

/// Measures one probe: the clip fraction of the samples themselves, the device's own overload flag
/// off the provenance, the channel's SNR, and the processed output — CRC-valid RDS groups per
/// second, through the same `AnalogReceiver` the workbench uses. Returns the PI it recovered too,
/// which the caller checks against the scene's truth only at the end.
fn measure(
    source: &mut dyn Source,
    probe: &GainProbe,
    pis: &mut Vec<(f64, Option<String>)>,
) -> Result<GainQuality, String> {
    let want = (probe.dwell_s * FS) as usize;
    let dwell = collect(source, want, &probe.state)?;
    let full = dwell
        .iq
        .iter()
        .map(|z| u64::from(z.re == 127 || z.re == -128) + u64::from(z.im == 127 || z.im == -128))
        .sum::<u64>();
    let clip_fraction = full as f64 / (2 * dwell.iq.len()) as f64;
    let session = AnalogReceiver::default()
        .run(
            info(&dwell.prov),
            &dwell.iq,
            &SnippetRequest {
                start_index: 0,
                end_index: dwell.iq.len() as u64,
                center_offset_hz: 0.0,
                bandwidth_hz: CHANNEL_BW,
            },
        )
        .map_err(|e| format!("the receiver refused the dwell: {e}"))?;
    let rds = session.rds();
    let groups = rds.map_or(0, |r| r.groups_ok);
    pis.push((
        probe.state.total_db(),
        rds.and_then(|r| r.pi.as_ref().map(|p| p.hex())),
    ));
    eprintln!(
        "[SIGNAL-062] probe {} {:>6} {} | clip {:.2e} overload {} | snr {:?} | rds {groups}",
        probe.index,
        probe.phase.as_str(),
        probe.state,
        clip_fraction,
        dwell.prov.get().overload,
        session
            .mode
            .features
            .snr_db
            .map(|v| (v * 10.0).round() / 10.0),
    );
    Ok(GainQuality::new(probe.state.clone(), probe.dwell_s)
        .with_clip(clip_fraction, Some(dwell.prov.get().overload))
        .with_snr(session.mode.features.snr_db)
        .with_decode(Some(DecodeQuality::from_count(
            "rds-groups",
            groups,
            probe.dwell_s,
        ))))
}

/// The scene behind the mock SDR, with the front end left in the explorer's overloaded state and a
/// `SourceLiveControl` over it — the real gated `DeviceAction` path.
fn front_end(dir: &TempDir) -> Option<(Box<dyn Source>, Arc<SourceLiveControl>)> {
    let rec = scene(&dir.0.join("rec"), 5.0)?;
    let driver = MockSdrDriver::new(
        &rec,
        MockOptions {
            block_len: 1 << 15,
            pacing: Pacing::Unpaced,
            end: hk_core::MockEnd::Loop,
            ..MockOptions::default()
        },
    )
    .unwrap();
    let source = driver.open_mock(&driver.default_request()).unwrap();
    let control = driver.last_control().unwrap();
    control.set_gains(&OVERLOADED).unwrap();
    let tuning = LiveTuning {
        center_hz: CENTER,
        sample_rate_hz: FS,
        gains: vec![
            NamedGain::new("lna", OVERLOADED.lna_db),
            NamedGain::new("vga", OVERLOADED.vga_db),
            NamedGain::new("amp", 11.0),
        ],
        bias_tee: BiasTee::Off,
        baseband_filter_hz: None,
    };
    Some((
        Box::new(source),
        Arc::new(SourceLiveControl::new(control, tuning)),
    ))
}

fn policy() -> GainPolicy {
    GainPolicy {
        enabled: true,
        dwell_s: 1.0,
        // The mock is unpaced, so there is no wall clock to wait out: the settle is the provenance
        // filter in `collect`, which refuses samples not served under the commanded state.
        settle_s: 0.0,
        coarse_probes: 5,
        fine_probes: 3,
        max_probes: 9,
        ..GainPolicy::default()
    }
}

#[test]
fn an_overloaded_scene_converges_on_a_better_decode_through_the_device_interface() {
    let dir = TempDir::new("t945-converge");
    let Some((mut source, control)) = front_end(&dir) else {
        return;
    };
    let mut manager = GainManager::new(control.clone(), policy()).unwrap();
    let started = manager.state_in_force();
    assert_eq!(started.total_db(), 73.0, "{started}");

    let mut pis: Vec<(f64, Option<String>)> = Vec::new();
    let report = manager
        .run(GainTrigger::Overload, |probe| {
            measure(source.as_mut(), probe, &mut pis)
        })
        .expect("the run finished")
        .expect("the policy is enabled, so a run happened");
    eprintln!("{}", report.explain());

    let rate = |s: &GainState| {
        report
            .probes
            .iter()
            .find(|p| p.applied.same_as(s))
            .and_then(|p| p.quality.decode.as_ref())
            .map(|d| d.rate_per_s)
            .unwrap_or_else(|| panic!("{s} was never probed"))
    };
    // The defect: the explorer's state decoded nothing and nothing moved the gain.
    assert_eq!(rate(&started), 0.0, "{}", report.explain());
    assert!(
        report.probes[0].quality.clip_fraction > 0.5,
        "the start state must really be saturated: {:?}",
        report.probes[0].quality
    );
    assert!(
        !report.probes[0].usable,
        "86 % of components pinned is not a state to measure from"
    );
    // The fix: it ended somewhere strictly better, and it is the state the device is left in.
    assert!(
        rate(&report.committed) > rate(&started),
        "committed {} is no better than the start:\n{}",
        report.committed,
        report.explain()
    );
    assert!(rate(&report.committed) >= 4.0, "{}", report.explain());
    assert!(
        report.committed.total_db() < started.total_db(),
        "{}",
        report.explain()
    );
    let left = GainState::new(control.tuning().gains);
    assert!(
        left.same_as(&report.committed),
        "the radio was left at {left}, not at the committed {}",
        report.committed
    );
    // Blind: the PI was never looked up, and it is the scene's own truth.
    let pi_at_best = pis
        .iter()
        .find(|(db, _)| (db - report.committed.total_db()).abs() < 1e-6)
        .and_then(|(_, pi)| pi.clone());
    assert_eq!(
        pi_at_best.as_deref(),
        Some(TRUTH_PI),
        "[SIGNAL-062] the committed state must recover the target's PI: {pis:?}"
    );
    assert!(
        pis.iter().all(|(db, pi)| *db < 60.0 || pi.is_none()),
        "a saturated state cannot have decoded a PI: {pis:?}"
    );

    // One device action per probe plus the commit, and nothing else reached the front end.
    assert_eq!(manager.device_actions(), report.probes.len() + 1);
    assert!(report.probes.len() <= policy().max_probes);
    for (i, a) in report.probes.iter().enumerate() {
        for b in &report.probes[i + 1..] {
            assert!(
                !a.applied.same_as(&b.applied),
                "probed {} twice:\n{}",
                a.applied,
                report.explain()
            );
        }
        // Provenance per interval: the dwell was served under the gains that probe commanded.
        assert!(a.applied.same_as(&a.quality.state), "{:?}", a.quality);
    }
    // The report says why, naming the metric that decided.
    assert!(report.why.contains("rds-groups"), "{}", report.why);
    assert!(
        report.why.contains("the state we started at"),
        "{}",
        report.why
    );
    // Settled: it holds rather than carrying on.
    assert_eq!(manager.controller().settled(), Some(&report.committed));
    let actions = manager.device_actions();
    assert!(
        manager
            .run(GainTrigger::Overload, |probe| measure(
                source.as_mut(),
                probe,
                &mut pis
            ))
            .unwrap()
            .is_none()
    );
    assert_eq!(manager.device_actions(), actions);
}

/// Today's behaviour, and the contrast the ticket is about: the flag is raised, nothing acts on it.
#[test]
fn with_the_policy_off_the_overloaded_front_end_is_left_exactly_where_it_was() {
    let dir = TempDir::new("t945-off");
    let Some((mut source, control)) = front_end(&dir) else {
        return;
    };
    let mut manager = GainManager::new(control.clone(), GainPolicy::default()).unwrap();
    let started = manager.state_in_force();
    let mut pis = Vec::new();
    assert!(
        manager
            .run(GainTrigger::Overload, |probe| measure(
                source.as_mut(),
                probe,
                &mut pis
            ))
            .unwrap()
            .is_none(),
        "the default policy must not run"
    );
    assert_eq!(manager.device_actions(), 0, "the radio was touched");
    let left = GainState::new(control.tuning().gains);
    assert!(left.same_as(&started), "{left} vs {started}");

    // And in that state the front end really is overloaded and really decodes nothing — which is
    // what the explorer saw, and what every gate stayed green through.
    let probe = GainProbe {
        state: started.clone(),
        settle_s: 0.0,
        dwell_s: 1.0,
        index: 0,
        budget: 1,
        phase: hk_core::gain::GainPhase::Start,
    };
    let q = measure(source.as_mut(), &probe, &mut pis).unwrap();
    assert_eq!(q.overload, Some(true), "the flag is raised");
    assert!(q.clip_fraction > 0.5, "{}", q.clip_fraction);
    assert_eq!(
        q.decode.as_ref().map(|d| d.rate_per_s),
        Some(0.0),
        "nothing decodes there"
    );
}
