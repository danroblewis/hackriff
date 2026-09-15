//! Compute providers in the pipeline (T-056, ADR-0007).
//!
//! **One [`Compute`] per run.** `Pipeline::start` takes [`PipelineSettings::compute`] (the plan's
//! `extra.pipeline.compute`, or `--compute` on `hk replay/run/serve` and `hackriffd`), applies the
//! `HK_COMPUTE*` environment over it ([`ComputeOptions::with_env`]: the environment wins), and
//! builds one [`Compute`]. Every segment of the run, re-plumbs included, shares it, so the GPU
//! context and its self-check are made once, before any reader starts. The provider cannot
//! change mid-run: selection depends only on those fixed options, on what is compiled in and
//! conformant, on the cached usability probe and on the FFT size. [`ComputeReport`] records each
//! reader's selection and counts `provider_changes`, which should stay 0. It is served under
//! `compute` in the run summary's counters and in `/api/status`.
//!
//! **Call sites.** The detection, history and spectrum readers build their STFTs through
//! [`stft`] (`Compute::stft`). No pipeline stage runs a polyphase channelizer (the chains use
//! per-channel DDCs), so no PFB is selected here. A stage that adds one takes it from
//! `Shared::compute` (`Compute::pfb`) and drains `PfbBackend::flush` at stream end.
//!
//! **Default builds** compile neither `gpu-wgpu` nor `accelerate`, so `auto` resolves STFT rows to
//! the CPU reference. Those are the same `CpuSpectral` rows `StftProcessor::new` builds, so frames
//! and detections are bit-identical.
//!
//! **Asynchronous GPU readback.** With `gpu-wgpu` compiled in, `auto` puts STFT rows on the GPU
//! with up to `gpu_in_flight` (default 2) batches in flight. A frame can then come out of a later
//! `push` than the one that completed it. Its contents, sample index, host time, discontinuity
//! flags and provenance are unchanged (the STFT replays its input events as rows arrive). Only
//! *when* the stage sees the frame moves, by at most `gpu_in_flight` ring chunks. The readers keep
//! that safe:
//! - **Stream end and detach.** At `Closed` (end of stream, stop, or the end of a segment on a
//!   re-plumb) each reader calls `StftProcessor::flush` before finishing its stage:
//!   - detection before the burst detector, tracker and writer finish;
//!   - history before the ingest queue is drained and the tiles are sealed;
//!   - spectrum before its publisher is finished.
//!
//!   The spectrum reader also flushes the old STFT under the old row plan before a display or
//!   rate change replaces it.
//! - **Stalled input.** When a read times out with nothing new (`Empty`, 50 ms) and rows are in
//!   flight, the reader flushes. A paused or starved source never holds frames back.
//! - **Timing.** Everything downstream keys on the frame's own sample index and host time, not
//!   on when it arrives: the floor, CFAR, the tracker, clip counts, the flush cadence and chain
//!   attach. Clips are kept until a frame passes them; the flush cadence runs on stream time;
//!   chains attach from the ring, which has seconds of pre-trigger reach. The lag (a few ms at
//!   20 Msps) is far inside the ring.
//!
//! [`PipelineSettings::compute`]: crate::PipelineSettings::compute

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use hk_dsp::compute::{Compute, ComputeOptions, Preference, ProviderKind, Selection};
use hk_dsp::{StftConfig, StftProcessor};
use serde_json::{Value, json};

/// An always-on reader that builds STFTs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reader {
    /// Reader 1: detection.
    Detect,
    /// Reader 2: history and the floor product.
    History,
    /// Reader 3: the spectrum stream.
    Spectrum,
}

impl Reader {
    /// The key under `compute.stft` (matches `readers.*` in the counters).
    fn key(self) -> &'static str {
        match self {
            Self::Detect => "detect",
            Self::History => "history",
            Self::Spectrum => "spectrum",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Detect => "detection",
            Self::History => "history",
            Self::Spectrum => "spectrum",
        }
    }
}

/// The run's compute selections (`compute` in the counters and `/api/status`).
#[derive(Debug, Default)]
pub struct ComputeReport {
    state: Mutex<ReportState>,
}

#[derive(Debug, Default)]
struct ReportState {
    options: Option<ComputeOptions>,
    providers: Vec<Value>,
    stft: BTreeMap<&'static str, Value>,
    pinned: BTreeMap<&'static str, ProviderKind>,
    builds: u64,
    provider_changes: u64,
}

impl ComputeReport {
    fn lock(&self) -> MutexGuard<'_, ReportState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records the run's options and provider availability. Probes usability (the GPU context and
    /// its self-check, the CPU pool) only when some workload asks for `auto` or a GPU, which also
    /// warms the GPU before the readers start; an explicit CPU or Accelerate run never touches
    /// the GPU.
    pub(crate) fn start(&self, compute: &Compute) {
        let opts = compute.options();
        let probe = [Some(opts.provider), opts.stft, opts.pfb]
            .into_iter()
            .flatten()
            .any(|p| matches!(p, Preference::Auto | Preference::Gpu | Preference::Cuda));
        let providers = if probe {
            compute
                .status()
                .into_iter()
                .map(|s| {
                    let (usable, detail) = match s.usable {
                        Ok(d) => (true, d),
                        Err(d) => (false, d),
                    };
                    json!({
                        "name": s.kind.name(),
                        "compiled": s.compiled,
                        "conformant": s.conformant,
                        "usable": usable,
                        "detail": detail,
                    })
                })
                .collect()
        } else {
            ProviderKind::ALL
                .iter()
                .map(|k| {
                    json!({
                        "name": k.name(),
                        "compiled": k.compiled(),
                        "conformant": k.conformant(),
                        "usable": Value::Null,
                        "detail": "not probed (no workload asks for auto or a GPU)",
                    })
                })
                .collect()
        };
        let mut st = self.lock();
        st.options = Some(opts.clone());
        st.providers = providers;
    }

    /// Records one STFT build of `reader`.
    fn record_stft(&self, reader: Reader, sel: &Selection, backend: &str, fft_len: usize) {
        let key = reader.key();
        let mut st = self.lock();
        st.builds += 1;
        match st.pinned.get(key).copied() {
            Some(first) if first != sel.provider => {
                st.provider_changes += 1;
                eprintln!(
                    "hk-pipeline compute: the {} STFT moved from {first} to {} mid-run ({sel})",
                    reader.label(),
                    sel.provider
                );
            }
            Some(_) => {}
            None => {
                st.pinned.insert(key, sel.provider);
            }
        }
        st.stft.insert(
            key,
            json!({
                "requested": sel.requested.to_string(),
                "provider": sel.provider.name(),
                "backend": backend,
                "fft_len": fft_len,
                "fallback": sel.fallback,
            }),
        );
    }

    /// The provider (`cpu`, `accelerate`, `gpu`, …) `reader` (`detect`, `history`, `spectrum`)
    /// last built its STFT on.
    pub fn stft_provider(&self, reader: &str) -> Option<String> {
        self.lock()
            .stft
            .get(reader)
            .and_then(|v| v["provider"].as_str())
            .map(str::to_owned)
    }

    /// STFT builds whose provider differed from that reader's first (0 for a correct run).
    pub fn provider_changes(&self) -> u64 {
        self.lock().provider_changes
    }

    /// A JSON snapshot.
    pub fn to_json(&self) -> Value {
        let st = self.lock();
        json!({
            "options": st.options,
            "providers": st.providers,
            "stft": st.stft,
            "stft_builds": st.builds,
            "provider_changes": st.provider_changes,
        })
    }
}

/// The run's [`Compute`] from `options` with the `HK_COMPUTE*` environment applied, recorded in
/// `report`. Returns the options in force.
pub(crate) fn for_run(
    options: &ComputeOptions,
    report: &ComputeReport,
) -> anyhow::Result<(Compute, ComputeOptions)> {
    let resolved = options
        .clone()
        .with_env()
        .map_err(|e| anyhow::anyhow!("compute options: {e}"))?;
    let compute = Compute::new(resolved.clone());
    report.start(&compute);
    Ok((compute, resolved))
}

/// An STFT for `reader` on the run's provider (see the module docs for flushing).
pub(crate) fn stft(
    compute: &Compute,
    report: &ComputeReport,
    reader: Reader,
    config: StftConfig,
) -> anyhow::Result<StftProcessor> {
    let (stft, sel) = compute
        .stft(config)
        .map_err(|e| anyhow::anyhow!("{} STFT: {e:?}", reader.label()))?;
    if crate::debug_enabled() {
        eprintln!(
            "hk-pipeline compute: {} {sel} ({})",
            reader.label(),
            stft.backend_name()
        );
    }
    report.record_stft(reader, &sel, stft.backend_name(), config.welch.fft_len);
    Ok(stft)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_dsp::WelchConfig;
    use hk_dsp::compute::Workload;

    #[test]
    fn selections_are_reported_and_a_provider_change_is_counted() {
        let report = ComputeReport::default();
        let compute = Compute::new(ComputeOptions {
            provider: Preference::Cpu,
            ..ComputeOptions::default()
        });
        report.start(&compute);
        let config = StftConfig::new(WelchConfig::new(256), 4);
        stft(&compute, &report, Reader::Detect, config).unwrap();
        stft(&compute, &report, Reader::Detect, config).unwrap();
        assert_eq!(report.stft_provider("detect").as_deref(), Some("cpu"));
        assert_eq!(report.provider_changes(), 0);
        let v = report.to_json();
        assert_eq!(v["stft_builds"], 2);
        assert_eq!(v["options"]["provider"], "cpu");
        assert_eq!(
            v["providers"][0]["usable"],
            Value::Null,
            "cpu is not probed"
        );

        let moved = Selection {
            workload: Workload::Stft,
            requested: Preference::Auto,
            provider: ProviderKind::GpuWgpu,
            fallback: None,
        };
        report.record_stft(Reader::Detect, &moved, "gpu-wgpu", 256);
        assert_eq!(report.provider_changes(), 1);
    }

    #[test]
    fn auto_resolves_to_the_cpu_reference_without_mac_providers() {
        if ProviderKind::GpuWgpu.compiled() || ProviderKind::Accelerate.compiled() {
            return;
        }
        let report = ComputeReport::default();
        let (compute, _) = for_run(&ComputeOptions::default(), &report).unwrap();
        if std::env::var_os("HK_COMPUTE").is_some() || std::env::var_os("HK_COMPUTE_STFT").is_some()
        {
            return;
        }
        let s = stft(
            &compute,
            &report,
            Reader::History,
            StftConfig::new(WelchConfig::new(4096), 4),
        )
        .unwrap();
        assert_eq!(
            s.backend_name(),
            StftProcessor::new(*s.config()).unwrap().backend_name()
        );
        assert_eq!(report.stft_provider("history").as_deref(), Some("cpu"));
    }
}
