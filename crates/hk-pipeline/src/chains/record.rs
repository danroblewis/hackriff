//! Pre-trigger recording (C25, ADR-0006): `TriggerStream` of `[trigger − pre, trigger + post)`
//! streamed to `recordings/<id>.sigmf-data` (ci8), one SigMF capture per provenance run, then a
//! Recording row. Only spawned under a class that permits content (the repository also refuses a
//! gated Recording). A window with missing samples is discarded, never spliced.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use hk_core::{ProvenanceHandle, TriggerRead, TriggerWindow};
use hk_model::sigmf::{Capture, Datatype, SigmfMeta};
use hk_model::{
    Recording, RecordingId, RecordingKind, RecordingTrigger, RetentionClass, TimeRange, Timestamp,
};
use num_complex::Complex;

use crate::gate::GateCursor;
use crate::run::Shared;
use crate::stats::inc;

/// ISO-8601 UTC with nanoseconds (`core:datetime`).
pub fn iso8601(t: Timestamp) -> String {
    let ns = t.as_unix_nanos();
    let secs = ns.div_euclid(1_000_000_000);
    let frac = ns.rem_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:09}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Claims the recording window's samples in the flow gate now (before the caller releases its
/// own claim), for a recording started on another thread with [`run_claimed`].
pub(crate) fn claim(shared: &Shared, trigger_sample: u64, pre_s: f64) -> GateCursor {
    let pre = (pre_s * shared.fs) as u64;
    shared.gate.register(trigger_sample.saturating_sub(pre))
}

/// [`run`] with a gate cursor already registered by [`claim`] (`None` registers one here).
pub(crate) fn run_claimed(
    shared: Arc<Shared>,
    cursor: Option<GateCursor>,
    trigger_sample: u64,
    pre_s: f64,
    post_s: f64,
    trigger: RecordingTrigger,
    label: String,
) {
    if let Err(e) = run_inner(
        &shared,
        cursor,
        trigger_sample,
        pre_s,
        post_s,
        trigger,
        &label,
    ) {
        inc(&shared.counters.chains.errors);
        eprintln!("hk-pipeline: recording ({label}): {e:#}");
    }
}

fn run_inner(
    shared: &Arc<Shared>,
    cursor: Option<GateCursor>,
    trigger_sample: u64,
    pre_s: f64,
    post_s: f64,
    trigger: RecordingTrigger,
    label: &str,
) -> anyhow::Result<()> {
    let fs = shared.fs;
    let window = TriggerWindow {
        trigger_sample,
        pre_samples: (pre_s * fs) as u64,
        post_samples: (post_s * fs) as u64,
    };
    let cursor = cursor.unwrap_or_else(|| shared.gate.register(window.start()));
    let mut stream = shared.ring.trigger_stream(window);
    let id = RecordingId::new();
    let dir = shared.cfg.data_dir.join("recordings");
    std::fs::create_dir_all(&dir)?;
    let stem = id.to_string();
    let data_path = dir.join(format!("{stem}.sigmf-data"));
    let meta_path = dir.join(format!("{stem}.sigmf-meta"));
    let mut out = BufWriter::new(
        File::create(&data_path).with_context(|| format!("creating {}", data_path.display()))?,
    );
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let mut bytes = Vec::new();
    let mut captures: Vec<Capture> = Vec::new();
    let mut first: Option<(Timestamp, ProvenanceHandle)> = None;
    let mut last_prov: Option<u64> = None;
    let mut delivered = 0u64;
    let mut missing = false;
    loop {
        match stream.wait(&mut buf, Duration::from_millis(50)) {
            TriggerRead::Data(c) => {
                let pid = c.provenance.id().to_string();
                let key = hk_detect::config::fnv1a64(pid.as_bytes());
                if last_prov != Some(key) {
                    last_prov = Some(key);
                    captures.push(Capture {
                        sample_start: delivered,
                        frequency: Some(c.provenance.tune.center_hz),
                        datetime: Some(iso8601(c.time.host_time)),
                        provenance: Some(c.provenance.get().clone()),
                        clip_count: None,
                        extra: serde_json::Map::new(),
                    });
                }
                first.get_or_insert((c.time.host_time, c.provenance.clone()));
                bytes.clear();
                for s in &buf[..c.len] {
                    bytes.push(s.re as u8);
                    bytes.push(s.im as u8);
                }
                out.write_all(&bytes)?;
                delivered += c.len as u64;
                cursor.set(c.end_sample());
            }
            TriggerRead::Missing { .. } => missing = true,
            TriggerRead::Pending { .. } => {}
            TriggerRead::Complete { .. } => break,
        }
    }
    out.flush()?;
    drop(out);
    drop(cursor);
    let c = &shared.counters.chains;
    let Some((t0, prov)) = first.filter(|_| !missing && delivered > 0) else {
        let _ = std::fs::remove_file(&data_path);
        inc(&c.recordings_incomplete);
        return Ok(());
    };
    let mut meta = SigmfMeta::new(Datatype::Ci8);
    meta.global.sample_rate = Some(fs);
    meta.global.description = Some(format!("hk-pipeline pre-trigger recording ({label})"));
    meta.global.recorder = Some("hk-pipeline".into());
    meta.global.hw = shared.cfg.device_hw.clone();
    meta.global.provenance = Some(prov.get().clone());
    meta.captures = captures;
    meta.write(&meta_path)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", meta_path.display()))?;
    // The trigger detection is written by the detection reader's next batched flush; wait for
    // its row (the Recording references it).
    if let RecordingTrigger::Detection(d) = trigger {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while shared.repo().detection(d).is_err() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let mut repo = shared.repo();
    let provenance_ref = repo.intern_provenance(prov.get())?;
    let rec = Recording {
        id,
        meta_uri: format!("recordings/{stem}.sigmf-meta"),
        data_uri: format!("recordings/{stem}.sigmf-data"),
        kind: RecordingKind::IqSnippet,
        time: TimeRange::new(
            t0,
            t0.saturating_add_nanos((delivered as f64 * 1e9 / fs) as i64),
        ),
        f_center_hz: prov.tune.center_hz,
        sample_rate_hz: fs,
        trigger,
        pre_trigger_s: pre_s,
        post_trigger_s: post_s,
        size_bytes: 2 * delivered,
        retention_class: RetentionClass::Unknown,
        content_class: shared.cfg.source_class,
        provenance_ref,
    };
    repo.insert_recording(&rec)?;
    inc(&c.recordings);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_round_trips_through_the_replay_parser() {
        for ns in [0i64, 1_789_300_800_123_456_789, 951_782_400_000_000_001] {
            let s = iso8601(Timestamp::from_unix_nanos(ns));
            let back = hk_core::source::sigmf_replay::parse_sigmf_datetime(&s).unwrap();
            assert_eq!(back.as_unix_nanos(), ns, "{s}");
        }
        assert_eq!(
            iso8601(Timestamp::from_unix_nanos(1_789_300_800 * 1_000_000_000)),
            "2026-09-13T12:00:00.000000000Z"
        );
    }
}
