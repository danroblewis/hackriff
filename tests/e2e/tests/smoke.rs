//! Smoke e2e tests, one per synthetic scenario: generate a small scenario, load it through the
//! harness, and check that the truth plumbing agrees with the samples. Real capability-stage
//! assertions arrive with each capability task (T-024 and on). Tests skip when `uv` is missing
//! (`HK_E2E_REQUIRE_SYNTH=1` makes that a failure).

use std::collections::BTreeSet;

use hk_e2e::paths::repo_root;
use hk_e2e::{
    BoxTolerance, Cf32, DetectionBox, Fixture, Pipeline, PipelineOutputs, Role, Stage, StageError,
    StageInput, SynthRequest, Tolerance, TruthItem, assert_param, match_detections, synth_or_skip,
};
use hk_model::TimestampMethod;
use hk_model::sigmf::Datatype;

const AWARE_036: &[&str] = &["AWARE-036"];
const AWARE_006: &[&str] = &["AWARE-006"];
const SPACE_050: &[&str] = &["SPACE-050"];
const AWARE_042: &[&str] = &["AWARE-042"];
const SIGNAL_062: &[&str] = &["SIGNAL-062"];
const SIGNAL_001: &[&str] = &["SIGNAL-001"];

fn mean_power(s: &[Cf32]) -> f64 {
    s.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / s.len() as f64
}

fn dbfs(p: f64) -> f64 {
    10.0 * p.log10()
}

fn undb(d: f64) -> f64 {
    10f64.powf(d / 10.0)
}

/// Power of a CW component at a known baseband offset (coherent projection).
fn tone_power_dbfs(s: &[Cf32], fs: f64, offset_hz: f64) -> f64 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (i, c) in s.iter().enumerate() {
        let (sin, cos) = (-2.0 * std::f64::consts::PI * offset_hz * i as f64 / fs).sin_cos();
        re += f64::from(c.re) * cos - f64::from(c.im) * sin;
        im += f64::from(c.re) * sin + f64::from(c.im) * cos;
    }
    let n = s.len() as f64;
    dbfs((re * re + im * im) / (n * n))
}

fn segment<'a>(s: &'a [Cf32], t: &TruthItem) -> &'a [Cf32] {
    &s[t.sample_start as usize..(t.sample_start + t.sample_count) as usize]
}

fn check_common(fx: &Fixture, scenario: &str, use_cases: &[&str]) {
    let prov = fx
        .meta
        .global
        .provenance
        .as_ref()
        .expect("global provenance");
    assert_eq!(prov.timestamp_method, TimestampMethod::Synthetic);
    for cap in &fx.meta.captures {
        assert!(
            cap.clip_count.is_some(),
            "{scenario}: capture without hackriff:clip_count"
        );
    }
    let sc = fx.scenario().expect("scenario truth annotation");
    assert_eq!(sc.str("scenario"), Some(scenario));
    assert_eq!(fx.use_cases(), use_cases);
    assert_eq!(fx.n_samples().unwrap(), sc.expect_f64("n_samples") as u64);
    assert!(
        !fx.with_role(Role::Floor).is_empty(),
        "{scenario}: no floor truth"
    );
    for t in &fx.truth {
        assert_ne!(t.role, Role::Unlabelled, "{scenario}: {t:?}");
        assert!(t.f_lo_hz <= t.f_hi_hz && t.t_start_s <= t.t_end_s);
    }
}

/// A test stage that emits fixed boxes, standing in for a detector.
struct OracleDetector {
    boxes: Vec<DetectionBox>,
}

impl Stage for OracleDetector {
    fn name(&self) -> &str {
        "oracle-detector"
    }

    fn run(&mut self, input: &StageInput<'_>, out: &mut PipelineOutputs) -> Result<(), StageError> {
        if !input.meta.annotations.is_empty() {
            return Err("truth leaked into the stage input".into());
        }
        if input.samples.is_empty() {
            return Err("no samples".into());
        }
        out.detections.extend(self.boxes.iter().cloned());
        Ok(())
    }
}

#[test]
fn tone_smoke() {
    let out = synth_or_skip!(SynthRequest::new("tone").seed(1));
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "tone", &[]);
    let s = fx.samples().unwrap();
    let tone = fx.of_kind("cw")[0];
    let fc = fx.center_hz_at(0).unwrap();
    let p = tone_power_dbfs(&s, fx.sample_rate, tone.expect_f64("center_hz") - fc);
    assert_param(
        &[],
        "tone power_dbfs",
        p,
        tone.expect_f64("power_dbfs"),
        Tolerance::Abs(0.3),
    );
    let floor = fx.with_role(Role::Floor)[0];
    assert_param(
        &[],
        "floor expected_floor_dbfs",
        dbfs(mean_power(&s) - undb(p)),
        floor.expect_f64("expected_floor_dbfs"),
        Tolerance::Abs(0.3),
    );

    let f32_out = synth_or_skip!(SynthRequest::new("tone").seed(1).datatype(Datatype::Cf32Le));
    let fx32 = f32_out.fixture(0).unwrap();
    assert_eq!(fx32.meta.global.datatype, Datatype::Cf32Le);
    let s32 = fx32.samples().unwrap();
    assert_eq!(s32.len(), s.len());
    let worst = s
        .iter()
        .zip(&s32)
        .map(|(a, b)| (a.re - b.re).abs().max((a.im - b.im).abs()))
        .fold(0.0f32, f32::max);
    assert!(worst <= 0.5 / 127.0 + 1e-6, "ci8 vs cf32 differ by {worst}");
}

#[test]
fn generation_is_cached_by_content_hash() {
    let request = SynthRequest::new("tone").seed(99).param("duration_s", 0.01);
    let first = synth_or_skip!(request);
    let stamp = || {
        std::fs::metadata(first.dir.join("manifest.json"))
            .unwrap()
            .modified()
            .unwrap()
    };
    let before = stamp();
    let second = synth_or_skip!(request);
    assert_eq!(first.dir, second.dir);
    assert_eq!(before, stamp(), "cached output was regenerated");
    assert!(first.dir.starts_with(hk_e2e::paths::synth_cache_dir()));
}

#[test]
fn fsk_burst_train_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(2)
            .param("duration_s", 0.3)
            .param("dc_offset_dbfs", -35)
            .param("iq_gain_db", 0.3)
    );
    assert_eq!(out.use_cases(), AWARE_036);
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "fsk_burst_train", AWARE_036);
    let sc = fx.scenario().unwrap();
    let bursts = fx.of_kind("fsk-burst");
    assert!(bursts.len() >= 2);
    assert_eq!(bursts.len() as f64, sc.expect_f64("/emitter/n_bursts"));
    assert_eq!(fx.of_kind("dc-offset").len(), 1);

    let s = fx.samples().unwrap();
    let n = s.len() as f64;
    let dc = s.iter().fold((0.0f64, 0.0f64), |(r, i), c| {
        (r + f64::from(c.re) / n, i + f64::from(c.im) / n)
    });
    let floor = undb(fx.with_role(Role::Floor)[0].expect_f64("expected_floor_dbfs"));
    for b in &bursts {
        assert_eq!(b.bool("/crc/valid"), Some(true));
        assert_eq!(b.identity(), Some(("sensor_id", "5a3c")));
        assert_eq!(b.str("modulation"), Some("2fsk"));
        assert_param(
            AWARE_036,
            "symbol_rate_bd",
            b.expect_f64("symbol_rate_bd"),
            sc.expect_f64("/emitter/symbol_rate_bd"),
            Tolerance::Rel(1e-12),
        );
        let seg = segment(&s, b);
        let power = seg
            .iter()
            .map(|c| (f64::from(c.re) - dc.0).powi(2) + (f64::from(c.im) - dc.1).powi(2))
            .sum::<f64>()
            / seg.len() as f64;
        assert_param(
            AWARE_036,
            "burst power_dbfs",
            dbfs(power - floor),
            b.expect_f64("power_dbfs"),
            Tolerance::Abs(0.7),
        );
    }

    // Stage plumbing: an oracle detector with slightly offset boxes plus one spurious box.
    let fc = fx.center_hz_at(0).unwrap();
    let mut boxes: Vec<DetectionBox> = bursts
        .iter()
        .map(|b| DetectionBox {
            t_start_s: b.t_start_s + 1e-3,
            t_end_s: b.t_end_s - 1e-3,
            f_lo_hz: b.f_lo_hz + 2e3,
            f_hi_hz: b.f_hi_hz + 2e3,
            snr_db: b.f64("snr_db"),
            flags: Vec::new(),
        })
        .collect();
    boxes.push(DetectionBox {
        t_start_s: 0.0,
        t_end_s: 0.01,
        f_lo_hz: fc - 200e3,
        f_hi_hz: fc - 190e3,
        ..Default::default()
    });
    let outputs = Pipeline::new()
        .with(OracleDetector { boxes })
        .run(&fx)
        .unwrap();
    assert_eq!(outputs.stages_run, ["oracle-detector"]);
    let report = match_detections(
        &outputs.detections,
        &bursts,
        &fx.artefacts(),
        BoxTolerance {
            time_s: 2e-3,
            freq_hz: 5e3,
        },
    );
    report.assert_all_found(AWARE_036, &bursts);
    assert_eq!(report.false_alarms, vec![bursts.len()]);
    report.assert_false_alarms_at_most(AWARE_036, &outputs.detections, 1);
}

#[test]
fn noise_floor_rise_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("noise_floor_rise")
            .seed(3)
            .param("duration_s", 0.1)
            .param("t0_s", 0.05)
            .param("n_weak_signals", 0)
    );
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "noise_floor_rise", AWARE_006);
    let events = fx.with_role(Role::Event);
    assert_eq!(events.len(), 1);
    let ev = events[0];
    assert_eq!(ev.str("expected_anomaly_kind"), Some("noise-floor-rise"));
    assert!((ev.center_hz() - 1575.42e6).abs() < 1.0);
    assert!(
        ev.str("t0_utc")
            .unwrap()
            .starts_with("2026-09-13T12:00:00.05")
    );

    let s = fx.samples().unwrap();
    let s0 = ev.sample_start as usize;
    let floor = |label: &str| {
        fx.with_role(Role::Floor)
            .into_iter()
            .find(|t| t.label.as_deref() == Some(label))
            .unwrap()
            .expect_f64("expected_floor_dbfs")
    };
    let before = dbfs(mean_power(&s[..s0]));
    let after = dbfs(mean_power(&s[s0..]));
    assert_param(
        AWARE_006,
        "floor before",
        before,
        floor("noise-floor-before"),
        Tolerance::Abs(0.3),
    );
    assert_param(
        AWARE_006,
        "floor after",
        after,
        floor("noise-floor-after"),
        Tolerance::Abs(0.3),
    );
    assert_param(
        AWARE_006,
        "step_db",
        after - before,
        ev.expect_f64("step_db"),
        Tolerance::Abs(0.4),
    );
}

#[test]
fn injected_floor_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("injected_floor")
            .seed(4)
            .param("segment_duration_s", 0.02)
            .param("n_cw_per_segment", 0)
    );
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "injected_floor", SPACE_050);
    let s = fx.samples().unwrap();
    let floors = fx.with_role(Role::Floor);
    assert_eq!(floors.len(), fx.meta.captures.len());
    assert!(floors.len() >= 6);
    for f in floors {
        let cap = fx.capture_at(f.sample_start).unwrap();
        let prov = cap.provenance.as_ref().expect("per-capture provenance");
        assert_eq!(Some(prov.tune.center_hz), cap.frequency);
        let measured_dbfs = dbfs(mean_power(segment(&s, f)));
        assert_param(
            SPACE_050,
            "segment floor dBFS",
            measured_dbfs,
            f.expect_f64("expected_floor_dbfs"),
            Tolerance::Abs(0.3),
        );
        assert_param(
            SPACE_050,
            "calibrated floor dBm",
            measured_dbfs + f.expect_f64("calibration_k_db"),
            f.expect_f64("floor_dbm"),
            Tolerance::Abs(1.0),
        );
    }
}

#[test]
fn occupancy_multi_hour_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("occupancy_multi_hour")
            .seed(5)
            .param("hours", 1)
            .param("windows", 2)
            .param("window_duration_s", 0.1)
    );
    let schedule = out.file_json("schedule.json").unwrap();
    let bursts = schedule["bursts"].as_array().unwrap();
    let windows = schedule["windows"].as_array().unwrap();
    let span = schedule["span_s"].as_f64().unwrap();
    assert_eq!(windows.len(), out.recordings.len());
    for ch in schedule["stats"]["per_channel"].as_array().unwrap() {
        let c = ch["channel"].as_u64().unwrap();
        let on: f64 = bursts
            .iter()
            .filter(|b| b["channel"].as_u64() == Some(c))
            .map(|b| b["duration_s"].as_f64().unwrap())
            .sum();
        assert_param(
            AWARE_042,
            "on_time_s",
            on,
            ch["on_time_s"].as_f64().unwrap(),
            Tolerance::Abs(1e-6),
        );
        assert_param(
            AWARE_042,
            "occupancy_fraction",
            on / span,
            ch["occupancy_fraction"].as_f64().unwrap(),
            Tolerance::Abs(1e-9),
        );
    }
    for (i, fx) in out.fixtures().unwrap().iter().enumerate() {
        check_common(fx, "occupancy_multi_hour", AWARE_042);
        let w0 = windows[i]["start_s"].as_f64().unwrap();
        assert_param(
            AWARE_042,
            "window start_s",
            fx.scenario().unwrap().expect_f64("/window/start_s"),
            w0,
            Tolerance::Abs(1.0 / fx.sample_rate),
        );
        let s = fx.samples().unwrap();
        let floor = undb(fx.with_role(Role::Floor)[0].expect_f64("expected_floor_dbfs"));
        let in_window = fx.of_kind("nbfm-burst");
        assert!(!in_window.is_empty(), "window {i} was placed on a burst");
        for b in in_window {
            let sched = &bursts[b.expect_f64("burst_index") as usize];
            assert_eq!(sched["channel"].as_f64(), b.f64("channel"));
            assert_eq!(sched["start_s"].as_f64(), b.f64("burst_start_s"));
            let seg = segment(&s, b);
            assert!(
                mean_power(seg) > floor + 0.5 * undb(b.expect_f64("power_dbfs")),
                "[AWARE-042] burst {} not present in window {i}",
                b.expect_f64("burst_index")
            );
        }
    }
}

#[test]
fn fm_broadcast_rds_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("fm_broadcast_rds")
            .seed(6)
            .param("duration_s", 0.3)
    );
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "fm_broadcast_rds", SIGNAL_062);
    let wfm = fx.of_kind("wfm-broadcast");
    assert_eq!(wfm.len(), 1);
    let wfm = wfm[0];
    assert_eq!(wfm.str("modulation"), Some("wfm"));
    assert_eq!(wfm.identity(), Some(("rds_pi", "C0DE")));
    assert_eq!(wfm.str("/rds/ps"), Some("HACKRIFF"));
    assert_eq!(wfm.f64("/pilot/frequency_hz"), Some(19_000.0));
    assert_eq!(wfm.f64("/rds/bitrate_bd"), Some(1187.5));
    let s = fx.samples().unwrap();
    let floor = undb(fx.with_role(Role::Floor)[0].expect_f64("expected_floor_dbfs"));
    assert_param(
        SIGNAL_062,
        "wfm power_dbfs",
        dbfs(mean_power(&s) - floor),
        wfm.expect_f64("power_dbfs"),
        Tolerance::Abs(0.3),
    );
}

#[test]
fn adsb_squitter_smoke() {
    let out = synth_or_skip!(
        SynthRequest::new("adsb_squitter")
            .seed(7)
            .param("duration_s", 0.1)
            .param("messages_per_aircraft", 4)
    );
    let fx = out.fixture(0).unwrap();
    check_common(&fx, "adsb_squitter", SIGNAL_001);
    let aircraft: BTreeSet<String> = fx
        .scenario()
        .unwrap()
        .get("aircraft")
        .and_then(|a| a.as_array())
        .unwrap()
        .iter()
        .map(|a| a["icao"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(aircraft.len(), 4);
    let msgs = fx.of_kind("adsb-df17");
    assert_eq!(msgs.len(), 16);

    let s = fx.samples().unwrap();
    let fs = fx.sample_rate;
    let mag = |pos: f64| {
        let i = pos.floor() as usize;
        let frac = pos - pos.floor();
        f64::from(s[i].norm_sqr()).sqrt() * (1.0 - frac)
            + f64::from(s[i + 1].norm_sqr()).sqrt() * frac
    };
    for m in msgs {
        assert_eq!(m.bool("/crc/valid"), Some(true));
        let (kind, icao) = m.identity().unwrap();
        assert_eq!(kind, "icao");
        assert!(aircraft.contains(icao), "{icao}");
        assert_eq!(m.str("message_hex").unwrap().len(), 28);
        let at = |us: f64| mag(m.sample_start as f64 + us * 1e-6 * fs);
        let pulses = [0.25, 1.25, 3.75, 4.75].map(at).iter().sum::<f64>() / 4.0;
        let gaps = [2.25, 2.75, 5.75, 6.75].map(at).iter().sum::<f64>() / 4.0;
        assert!(
            pulses > 3.0 * gaps,
            "[SIGNAL-001] preamble of {icao} at sample {} not visible",
            m.sample_start
        );
    }
}

#[test]
fn committed_tiny_fixture_loads_without_role_keys() {
    let fx = Fixture::load(repo_root().join("fixtures/tiny/tone.sigmf-meta")).unwrap();
    assert_eq!(fx.truth.len(), 1);
    let t = &fx.truth[0];
    assert_eq!(t.role, Role::Unlabelled);
    assert_eq!(t.kind, "cw");
    assert_eq!(t.f64("frequency_hz"), Some(100_010_000.0));
    let data = std::fs::read(fx.data_path()).unwrap();
    if data.starts_with(b"version https://git-lfs") {
        eprintln!("SKIP sample check: fixture data is an unfetched Git LFS pointer");
        return;
    }
    assert_eq!(fx.samples().unwrap().len(), 4096);
}
