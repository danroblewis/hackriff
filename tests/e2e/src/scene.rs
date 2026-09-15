//! Time-compressed scenes (T-125): a long simulated scene served through the mock SDR as its
//! short IQ windows, on the scene's own clock.
//!
//! `occupancy_markov_scene` (T-117) draws a 24–48 h truth schedule and renders only short IQ
//! windows (with `iq_windows_at_revisits=true`, one per observation-schedule revisit).
//! [`join_scene_windows`] turns them into **one** multi-capture SigMF recording the mock SDR
//! device serves like live air:
//!
//! - one capture per window, in schedule order, each keeping its `core:datetime`;
//! - `core:global_index` = the window's start sample on the scene clock (relative to the first
//!   window), so the time between windows is a recording gap. The mock serves it as a `GAP` whose
//!   `dropped_before` is the missing duration: stream time jumps to the next window, no hours of
//!   silence are synthesised, and blocks never straddle two windows;
//! - no annotations and no description: the recording carries no truth. The blind harness strips
//!   and checks it again before the device opens it.
//!
//! The hidden truth (`schedule.json`) stays with the test: [`SceneTruth`] is read only by
//! assertions, after the run.

use std::path::{Path, PathBuf};

use hk_model::sigmf::SigmfMeta;
use serde_json::{Value, json};

use crate::synth::{SynthError, SynthOutput};

fn bad(path: &Path, message: impl Into<String>) -> SynthError {
    SynthError::BadOutput {
        path: path.to_owned(),
        message: message.into(),
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> SynthError + '_ {
    move |source| SynthError::Io {
        path: path.to_owned(),
        source,
    }
}

/// A scene recording written by [`join_scene_windows`].
#[derive(Clone, Debug)]
pub struct SceneRecording {
    /// The joined, truth-free `.sigmf-meta`.
    pub meta: PathBuf,
    /// Windows joined.
    pub windows: usize,
    /// IQ samples in the recording (all windows).
    pub samples: u64,
    /// Scene seconds from the first window's start to the last window's end.
    pub simulated_span_s: f64,
    /// Scene-clock samples between windows (the device serves them as gaps, never as IQ).
    pub gap_samples: u64,
}

/// Joins the scene's rendered windows (`schedule.json` `windows`, in start order) into
/// `<dir>/<name>.sigmf-meta` + `.sigmf-data`, one capture per window at its scene-clock sample
/// index. See the [module docs](self).
pub fn join_scene_windows(
    out: &SynthOutput,
    dir: &Path,
    name: &str,
) -> Result<SceneRecording, SynthError> {
    let schedule_path = out.dir.join("schedule.json");
    let schedule = out.file_json("schedule.json")?;
    let mut windows: Vec<(f64, PathBuf)> = schedule["windows"]
        .as_array()
        .ok_or_else(|| bad(&schedule_path, "no windows array"))?
        .iter()
        .map(|w| {
            let start = w["start_s"].as_f64();
            let rec = w["recording"].as_str();
            match (start, rec) {
                (Some(s), Some(r)) => Ok((s, out.dir.join(r))),
                _ => Err(bad(&schedule_path, format!("malformed window {w}"))),
            }
        })
        .collect::<Result<_, _>>()?;
    if windows.is_empty() {
        return Err(bad(&schedule_path, "the scene rendered no windows"));
    }
    windows.sort_by(|a, b| a.0.total_cmp(&b.0));

    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let first_meta =
        SigmfMeta::read(&windows[0].1).map_err(|e| bad(&windows[0].1, e.to_string()))?;
    let fs = first_meta
        .global
        .sample_rate
        .ok_or_else(|| bad(&windows[0].1, "no core:sample_rate"))?;
    let bytes_per_sample = first_meta.global.datatype.bytes_per_sample();
    let index_of = |s: f64| (s * fs).round() as u64;
    let g0 = index_of(windows[0].0);

    let mut meta = first_meta.clone();
    meta.captures.clear();
    meta.annotations.clear();
    meta.global.description = None;
    let mut data = Vec::new();
    let mut end_s = 0.0;
    let mut last_index_end = 0u64;
    for (start_s, path) in &windows {
        let m = SigmfMeta::read(path).map_err(|e| bad(path, e.to_string()))?;
        if m.global.datatype != first_meta.global.datatype || m.global.sample_rate != Some(fs) {
            return Err(bad(path, "windows differ in datatype or sample rate"));
        }
        let bytes = std::fs::read(m_data(path)).map_err(io(path))?;
        let n = (bytes.len() / bytes_per_sample) as u64;
        let index = index_of(*start_s) - g0;
        if index < last_index_end {
            return Err(bad(path, "scene windows overlap"));
        }
        let offset = (data.len() / bytes_per_sample) as u64;
        for cap in &m.captures {
            let mut c = cap.clone();
            c.extra
                .insert("core:global_index".into(), json!(index + cap.sample_start));
            c.sample_start += offset;
            meta.captures.push(c);
        }
        data.extend_from_slice(&bytes);
        last_index_end = index + n;
        end_s = start_s + n as f64 / fs;
    }
    let out_meta = dir.join(format!("{name}.sigmf-meta"));
    meta.write(&out_meta)
        .map_err(|e| bad(&out_meta, e.to_string()))?;
    let out_data = out_meta.with_extension("sigmf-data");
    std::fs::write(&out_data, &data).map_err(io(&out_data))?;
    let samples = (data.len() / bytes_per_sample) as u64;
    Ok(SceneRecording {
        meta: out_meta,
        windows: windows.len(),
        samples,
        simulated_span_s: end_s - windows[0].0,
        gap_samples: last_index_end - samples,
    })
}

fn m_data(meta: &Path) -> PathBuf {
    meta.with_extension("sigmf-data")
}

/// One scene channel from the hidden schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneChannel {
    /// Channel number.
    pub channel: u64,
    /// `markov`, `diurnal`, `novelty`, `event` or `boring`.
    pub kind: String,
    /// Centre, Hz.
    pub center_hz: f64,
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
}

/// The hidden truth of an `occupancy_markov_scene` (`schedule.json`). Tests read it only in
/// assertions, after the run.
#[derive(Clone, Debug)]
pub struct SceneTruth {
    /// The raw schedule.
    pub schedule: Value,
    /// Simulated span, s.
    pub span_s: f64,
    /// Channels.
    pub channels: Vec<SceneChannel>,
    /// Scene seconds at which the novelty emitter first may transmit.
    pub novelty_start_s: f64,
    /// Rendered window starts, scene seconds, ascending.
    pub window_starts_s: Vec<f64>,
}

impl SceneTruth {
    /// Reads `schedule.json` from `out`.
    pub fn load(out: &SynthOutput) -> Result<Self, SynthError> {
        let path = out.dir.join("schedule.json");
        let schedule = out.file_json("schedule.json")?;
        let f = |v: &Value, k: &str| {
            v[k].as_f64()
                .ok_or_else(|| bad(&path, format!("missing number {k}")))
        };
        let channels = schedule["channels"]
            .as_array()
            .ok_or_else(|| bad(&path, "no channels"))?
            .iter()
            .map(|c| {
                Ok(SceneChannel {
                    channel: c["channel"].as_u64().unwrap_or(u64::MAX),
                    kind: c["kind"].as_str().unwrap_or_default().to_owned(),
                    center_hz: f(c, "center_hz")?,
                    bandwidth_hz: f(c, "bandwidth_hz")?,
                })
            })
            .collect::<Result<_, SynthError>>()?;
        let mut window_starts_s: Vec<f64> = schedule["windows"]
            .as_array()
            .map(|w| w.iter().filter_map(|w| w["start_s"].as_f64()).collect())
            .unwrap_or_default();
        window_starts_s.sort_by(f64::total_cmp);
        Ok(Self {
            span_s: f(&schedule, "span_s")?,
            novelty_start_s: f(&schedule["novelty"], "start_s")?,
            channels,
            window_starts_s,
            schedule,
        })
    }

    /// The first channel of `kind`.
    pub fn channel(&self, kind: &str) -> Option<&SceneChannel> {
        self.channels.iter().find(|c| c.kind == kind)
    }

    /// Truth intervals `(start_s, end_s)` of `channel`.
    pub fn intervals(&self, channel: u64) -> Vec<(f64, f64)> {
        self.schedule["intervals"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|iv| iv["channel"].as_u64() == Some(channel))
                    .filter_map(|iv| {
                        let s = iv["start_s"].as_f64()?;
                        Some((s, s + iv["duration_s"].as_f64()?))
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
