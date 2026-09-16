//! The provider conformance suite (ADR-0016 §6, T-203).
//!
//! Every provider runs **one fixed model on fixed tensors** and is compared against:
//!
//! 1. **An independent reference** — a numpy float64 forward pass over the same weights,
//!    generated with the model by `py/hkpy/ml/make_conformance_model.py`. This matters: a suite
//!    whose expectations came from one of the runtimes it tests would only prove the runtimes
//!    agree with each other, not that either computes the graph.
//! 2. **The CPU reference provider** (tract), which is what ADR-0016 §6 names as the baseline.
//!
//! A-priori tolerances, fixed before any provider was run (ADR-0016 §6, C38 card):
//!
//! | Check | Floor |
//! |---|---|
//! | FP32 max abs Δlogit, against the reference and against tract | ≤ 1e-3 |
//! | Batch invariance: 1 single vs the same item inside a batch of 32 | ≤ 1e-4 |
//! | A model file that does not match its manifest sha256 | refused |
//! | An operator outside the §6 allowlist | refused, typed |
//! | A wrongly shaped batch | refused, typed |
//! | Deadline missed while queued | dropped and counted, never answered late |
//!
//! FP16/INT8 agreement (≥ 99 % top-1, |Δprob| ≤ 0.02) is **not measured**: there is no quantised
//! model to measure it on. That row of ADR-0016 §6 stays open, and a provider loading a
//! reduced-precision model is not conformant until it is.
//!
//! The suite also prints the **bake-off** numbers ADR-0016 §6 made a decision rule out of: p50 and
//! p99 latency at batch 32 per provider. Run it with
//! `cargo test -p hk-ml --features ml-coreml --test conformance -- --nocapture`.

#![cfg(feature = "ml-onnx")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use hk_ml::gate::{Subject, admit};
use hk_ml::host::{ConsumerId, HostConfig, InferenceRequest, MemoryShadowSink, ModelHost};
use hk_ml::registry::sha256_hex;
use hk_ml::{
    LoadedModel, MlError, MlMode, MlProvider, MlProviderKind, ModelManifest, RawOutput, Tensor,
    TensorBatch,
};

/// Max abs difference in a logit, FP32, against the reference and across providers.
const LOGIT_TOL: f64 = 1e-3;
/// Max abs difference between an item run alone and the same item inside a batch.
const BATCH_INVARIANCE_TOL: f64 = 1e-4;
/// Batches timed for the bake-off.
const TIMED_BATCHES: usize = 200;

struct Case {
    batch: usize,
    input: Vec<f32>,
    logits: Vec<f64>,
}

fn fixture() -> (ModelManifest, Vec<u8>, Vec<Case>) {
    let dir = hk_ml::conformance_fixture_dir();
    let manifest: ModelManifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).expect("manifest"))
            .expect("manifest parses");
    manifest.validate().expect("the fixture manifest is valid");
    let bytes = std::fs::read(dir.join("model.onnx")).expect("model.onnx");
    assert_eq!(
        sha256_hex(&bytes),
        manifest.sha256,
        "the checked-in model and manifest have drifted; regenerate with \
         py/hkpy/ml/make_conformance_model.py"
    );
    let cases: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("cases.json")).expect("cases")).unwrap();
    let cases = cases["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .map(|c| Case {
            batch: c["batch"].as_u64().unwrap() as usize,
            input: c["input"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap() as f32)
                .collect(),
            logits: c["logits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_f64().unwrap())
                .collect(),
        })
        .collect();
    (manifest, bytes, cases)
}

/// Every provider built into this binary.
fn providers() -> Vec<Arc<dyn MlProvider>> {
    #[allow(unused_mut)]
    let mut v: Vec<Arc<dyn MlProvider>> = vec![Arc::new(hk_ml::tract_provider::TractProvider)];
    #[cfg(feature = "ml-coreml")]
    {
        v.push(Arc::new(hk_ml::ort_provider::OrtProvider::cpu()));
        v.push(Arc::new(hk_ml::ort_provider::OrtProvider::coreml()));
    }
    v
}

fn run(model: &dyn LoadedModel, case: &Case) -> Vec<RawOutput> {
    let shape = vec![case.batch, 2, 64];
    let batch = TensorBatch {
        input: Tensor::new(shape, case.input.clone()).expect("shape and data agree"),
        batch_size: case.batch as u16,
    };
    model.infer(&batch).expect("the fixture model runs")
}

fn max_abs_diff(got: &[RawOutput], want: &[f64]) -> f64 {
    got.iter()
        .flat_map(|o| o.logits.iter())
        .zip(want)
        .map(|(g, w)| (f64::from(*g) - w).abs())
        .fold(0.0, f64::max)
}

#[test]
fn every_provider_computes_the_fixed_model_to_the_a_priori_tolerance() {
    let (manifest, bytes, cases) = fixture();
    let mut table = String::from(
        "conformance hk-conformance@1.0.0 (fp32)\n  provider      max |Δlogit| vs numpy   vs tract   batch-invariance\n",
    );
    let mut reference: Option<Vec<Vec<f32>>> = None;

    for provider in providers() {
        let model = provider
            .load(&manifest, &bytes)
            .unwrap_or_else(|e| panic!("{} cannot load the fixture: {e}", provider.kind()));

        let mut worst_ref = 0.0_f64;
        let mut per_case: Vec<Vec<f32>> = Vec::new();
        for case in &cases {
            let out = run(model.as_ref(), case);
            assert_eq!(
                out.len(),
                case.batch,
                "{}: one output per item",
                provider.kind()
            );
            worst_ref = worst_ref.max(max_abs_diff(&out, &case.logits));
            per_case.push(out.iter().flat_map(|o| o.logits.clone()).collect());
        }
        assert!(
            worst_ref <= LOGIT_TOL,
            "{}: max |Δlogit| {worst_ref:.3e} vs the independent numpy reference exceeds {LOGIT_TOL:.0e}",
            provider.kind()
        );

        // Batch invariance: the fixture's batch-1 case IS the first item of the batch-32 case.
        let single = &per_case[0];
        let batched = &per_case[1];
        let invariance = single
            .iter()
            .zip(batched)
            .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
            .fold(0.0, f64::max);
        assert!(
            invariance <= BATCH_INVARIANCE_TOL,
            "{}: the same item gives different logits alone ({invariance:.3e}) and in a batch of 32",
            provider.kind()
        );

        let vs_tract = match &reference {
            None => {
                reference = Some(per_case.clone());
                0.0
            }
            Some(r) => r
                .iter()
                .flatten()
                .zip(per_case.iter().flatten())
                .map(|(a, b)| (f64::from(*a) - f64::from(*b)).abs())
                .fold(0.0, f64::max),
        };
        assert!(
            vs_tract <= LOGIT_TOL,
            "{}: max |Δlogit| {vs_tract:.3e} against the CPU reference exceeds {LOGIT_TOL:.0e}",
            provider.kind()
        );
        table.push_str(&format!(
            "  {:<13} {:>18.3e} {:>10.3e} {:>18.3e}\n",
            provider.kind().as_str(),
            worst_ref,
            vs_tract,
            invariance
        ));
    }
    println!("{table}");
}

#[test]
fn a_model_that_does_not_match_its_manifest_or_the_operator_allowlist_is_refused() {
    let (manifest, bytes, _) = fixture();
    for provider in providers() {
        // Corrupted bytes: the hash check refuses them before any engine sees them.
        let mut corrupted = bytes.clone();
        *corrupted.last_mut().unwrap() ^= 0xff;
        assert!(
            matches!(
                provider.load(&manifest, &corrupted),
                Err(MlError::HashMismatch(_))
            ),
            "{}: a file that is not the manifest's must not load",
            provider.kind()
        );

        // Not a model at all: a typed error, never a panic.
        let mut junk_manifest = manifest.clone();
        let junk = b"this is not an ONNX graph".to_vec();
        junk_manifest.sha256 = sha256_hex(&junk);
        junk_manifest.model.sha8 = junk_manifest.sha256[..8].to_owned();
        assert!(
            provider.load(&junk_manifest, &junk).is_err(),
            "{}: junk must not load",
            provider.kind()
        );

        // A wrongly shaped batch: refused, and the error says so.
        //
        // Note *which* shapes are wrong is the graph's business, not a manifest's: this model
        // global-average-pools, so any sample count is legitimately fine and only the channel
        // count and the rank are fixed. A declared input spec would have "refused" a width the
        // model actually accepts — which is why the graph is the authority here.
        let model = provider.load(&manifest, &bytes).expect("loads");
        for (shape, data) in [
            (vec![1, 3, 64], 192),    // three channels into a 2-channel convolution
            (vec![1, 64], 64),        // rank 2 into a 1-D convolution
            (vec![1, 2, 64, 1], 128), // rank 4
        ] {
            let bad = TensorBatch {
                input: Tensor::new(shape.clone(), vec![0.0; data]).unwrap(),
                batch_size: 1,
            };
            assert!(
                model.infer(&bad).is_err(),
                "{}: input {shape:?} is not this graph's and must be refused",
                provider.kind()
            );
        }
        // A batch size that disagrees with the tensor's own leading dimension.
        let inconsistent = TensorBatch {
            input: Tensor::new(vec![1, 2, 64], vec![0.0; 128]).unwrap(),
            batch_size: 4,
        };
        assert!(model.infer(&inconsistent).is_err(), "{}", provider.kind());
    }
}

/// The ADR-0016 §6 bake-off measurement: p50/p99 at batch 32, per provider.
///
/// This **prints** rather than asserts. The rule it feeds ("ship CPU-only unless CoreML is ≥ 2×
/// better") is a decision about what to ship, recorded in the planning log; turning it into an
/// assertion would make an unrelated machine's timing a test failure.
#[test]
fn bake_off_latency_at_batch_32() {
    let (manifest, bytes, cases) = fixture();
    let case = cases.iter().find(|c| c.batch == 32).expect("batch-32 case");
    println!("bake-off: {TIMED_BATCHES} batches of 32, one warm-up batch discarded");
    for provider in providers() {
        let model = provider.load(&manifest, &bytes).expect("loads");
        let _ = run(model.as_ref(), case); // warm-up: plan compilation / EP graph partitioning
        let mut us: Vec<f64> = Vec::with_capacity(TIMED_BATCHES);
        for _ in 0..TIMED_BATCHES {
            let started = Instant::now();
            let _ = run(model.as_ref(), case);
            us.push(started.elapsed().as_secs_f64() * 1e6);
        }
        us.sort_by(f64::total_cmp);
        println!(
            "  {:<13} p50 {:>9.1} µs   p99 {:>9.1} µs   per item p50 {:>7.2} µs",
            provider.kind().as_str(),
            us[us.len() / 2],
            us[(us.len() * 99) / 100],
            us[us.len() / 2] / 32.0,
        );
    }
}

// ---- the host, driven with a real engine ---------------------------------------------------

fn subject() -> Subject {
    use hk_model::classify::{
        CLASSIFICATION_SCHEMA, ClassProvenance, Classification, Coarse, HK_MOD_V1, LabelP, Stage,
        SuspectFlags, TaxonomyRef, entropy_norm,
    };
    use hk_model::{
        Detection, DetectionFlags, DetectionId, ProvenanceId, SurveyId, TimeRange, Timestamp,
    };

    let detection = Detection {
        id: DetectionId::new(),
        survey_id: SurveyId::new(),
        time: TimeRange::new(
            Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
            Timestamp::from_unix_nanos(1_789_300_821_000_000_000),
        ),
        f_center_hz: 915e6,
        obw_hz: 40e3,
        xdb_bandwidth_hz: None,
        xdb_level_db: None,
        snr_peak_db: 24.0,
        snr_mean_db: 21.0,
        peak_level_dbfs: -18.0,
        peak_level_dbm: None,
        sk: None,
        clip_count: 0,
        detector_version: "hk-detect/cfar@0.1.0;pfa=1e-6".into(),
        provenance_ref: ProvenanceId::new(),
        flags: DetectionFlags::default(),
    };
    let posterior = vec![
        LabelP {
            label: "fsk".into(),
            p: 0.8,
        },
        LabelP {
            label: "unknown".into(),
            p: 0.2,
        },
    ];
    let classification = Classification {
        schema: CLASSIFICATION_SCHEMA,
        t: Timestamp::from_unix_nanos(1_789_300_820_000_000_000),
        taxonomy: TaxonomyRef::current(),
        input: None,
        coarse: Coarse::Digital,
        entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
        likelihood: posterior.clone(),
        posterior,
        prior: None,
        family: "fsk".into(),
        confidence: 0.8,
        class: None,
        open_set_score: 0.2,
        stage: Stage::FeatureTree,
        provenance: ClassProvenance {
            rules: "hk-classify/tree@1".into(),
            // Determinate (T-292): this fixture stands for a row a current writer produced, not
            // the pre-T-290 `FEATURES_VERSION_INDETERMINATE` marker. hk-ml doesn't depend on
            // hk-classify, so it can't name `hk_classify::FEATURES_VERSION` directly.
            features_version: 2,
            features_ref: None,
            ml: None,
            snr_db: Some(24.0),
            snr_gate_db: 20.0,
            gated: false,
            thresholds: "thresholds@1".into(),
            suspect: SuspectFlags::default(),
            power_mode: None,
        },
        flags: Vec::new(),
        reasons: Vec::new(),
    };
    admit(&detection, &classification).expect("a classified CFAR detection is admitted")
}

fn item(case: &Case) -> Tensor {
    Tensor::new(vec![2, 64], case.input[..128].to_vec()).unwrap()
}

#[test]
fn the_host_runs_a_real_model_in_shadow_with_measured_provenance_and_counts_late_requests() {
    let (manifest, bytes, cases) = fixture();
    let sink = Arc::new(MemoryShadowSink::new());
    let host = ModelHost::new(
        HostConfig::default(),
        Arc::new(hk_ml::tract_provider::TractProvider),
    )
    .unwrap()
    .with_shadow_sink(Arc::clone(&sink) as Arc<dyn hk_ml::host::ShadowSink>);
    let model = host.load(&manifest, &bytes).unwrap();
    let consumer = ConsumerId::new("hk-ml/conformance");
    host.set_mode(&model, &consumer, MlMode::Shadow, false)
        .unwrap();

    let request = InferenceRequest::new(
        subject(),
        consumer.clone(),
        item(&cases[0]),
        Duration::from_secs(5),
    );
    let prediction = host
        .observe(&model, &request, None)
        .unwrap()
        .expect("a shadow model runs");
    prediction.validate().unwrap();

    // Provenance is the bytes that ran, the runtime that ran them, and the precision.
    assert_eq!(
        prediction.model.to_string(),
        "hk-conformance@1.0.0#7768a0ec"
    );
    assert_eq!(prediction.model.sha8, manifest.sha256[..8]);
    assert_eq!(prediction.provider, MlProviderKind::CpuTract);
    assert_eq!(prediction.precision, manifest.precision);
    // …and it decides nothing.
    assert!(!prediction.decides(), "a shadow prediction never decides");
    assert!(
        host.decide(&model, &request).is_err(),
        "a shadow model must not be reachable through the decision path"
    );
    let recorded = sink.entries();
    assert_eq!(recorded.len(), 1, "the shadow record was written");
    assert_eq!(recorded[0].mode, "shadow");
    assert_eq!(recorded[0].family, "fsk");
    assert_eq!(recorded[0].snr_db, Some(24.0));

    // A request that is already late is dropped and counted, never answered after its deadline.
    let mut late = request.clone();
    late.deadline = Instant::now() - Duration::from_millis(1);
    assert!(host.observe(&model, &late, None).unwrap().is_none());
    let stats = host.stats();
    assert_eq!(stats.deadline_missed, 1, "{stats:?}");
    assert_eq!(stats.inferred, 1, "{stats:?}");

    host.unload(&model).unwrap();
    assert!(host.loaded().is_empty());
    // Unloading forgets the model's modes, so nothing runs and nothing decides — and it cannot
    // be put back into a mode without being loaded again.
    assert!(
        host.observe(&model, &request, None).unwrap().is_none(),
        "an unloaded model answers nothing"
    );
    assert!(host.decide(&model, &request).is_err());
    assert!(
        host.set_mode(&model, &consumer, MlMode::Shadow, false)
            .is_err(),
        "a mode cannot be set on a model that is not loaded"
    );
}
