//! Shared helpers for hk-dsp integration tests.
#![allow(dead_code)]

use hk_core::{BlockHeader, Discontinuity, ProvenanceHandle};
use hk_model::{Provenance, SampleTime, Timestamp};

/// A synthetic provenance record. Built from JSON so the test does not depend on every field
/// of the record (unknown keys are ignored when the model drops one).
pub fn provenance_with(center_hz: f64, sample_rate_hz: f64, lna_db: f64) -> ProvenanceHandle {
    let json = serde_json::json!({
        "device_id": "synthetic:hk-dsp-test",
        "tune": {
            "center_hz": center_hz,
            "sample_rate_hz": sample_rate_hz,
            "lna_db": lna_db,
            "vga_db": 20.0,
            "amp_on": false,
            "bandwidth_hz": sample_rate_hz * 0.75,
        },
        "clip_count": 0,
        "overload": false,
        "clock_source": "internal",
        "clock_locked": true,
        "timestamp_method": "synthetic",
        "timestamp_error_budget_ns": 0,
    });
    let p: Provenance = serde_json::from_value(json).expect("provenance JSON");
    ProvenanceHandle::new(p)
}

pub fn provenance(center_hz: f64, sample_rate_hz: f64) -> ProvenanceHandle {
    provenance_with(center_hz, sample_rate_hz, 16.0)
}

/// Header for a block starting at stream index `first`; host time is `first / fs` seconds.
pub fn header(first: u64, prov: &ProvenanceHandle, flags: Discontinuity) -> BlockHeader {
    let fs = prov.tune.sample_rate_hz;
    BlockHeader {
        time: SampleTime {
            sample_index: first,
            host_time: Timestamp::from_unix_nanos((first as f64 * 1e9 / fs).round() as i64),
        },
        provenance: prov.clone(),
        discontinuity: flags,
        dropped_before: 0,
    }
}

pub fn db(x: f64) -> f64 {
    10.0 * x.log10()
}

pub fn mean(v: &[f32]) -> f64 {
    v.iter().map(|&x| f64::from(x)).sum::<f64>() / v.len() as f64
}
