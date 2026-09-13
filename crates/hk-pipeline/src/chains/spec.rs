//! Chain specs: data, not code (ADR-0001 S1, ADR-0003 routing).
//!
//! A [`ChainSpec`] says *when* a runtime chain attaches (a trigger plus frequency, bandwidth,
//! burstiness, channel-raster and member-count priors) and *what* it runs (a node list). The
//! registry is JSON: the built-in one ([`BUILTIN_CHAINS`]) or `ScanPlan.extra.pipeline.chains`.
//! Adding a decoder is a manifest plus a spec entry; nothing is recompiled and capture never
//! restarts.
//!
//! Node lists are validated into one of three shapes ([`ChainShape`]):
//! - `[record?] analog-auto` — C19 auto mode (estimate → mode → WFM + RDS); writes content, so
//!   `requires_content` must be set. A short probe window runs mode selection first; the chain
//!   continues only when the mode is accepted (`accept_modes`, `require_pilot`);
//! - `[record?] fsk-bursts` — C13/C14 blind estimate, C20 demod per burst, C21 framing; content
//!   fails closed unless the emitter is classified;
//! - `[record?] ddc? plugin` — DDC to the plugin's rate, then a subprocess decoder (ADR-0003).
//!
//! **Channel priors.** A spec with `raster_hz` snaps a candidate's centre to the band's channel
//! raster and widens it to the node's channel bandwidth, and at most one chain runs per channel.
//! This is how a WFM station the detector covers with several narrow boxes (its noisy 200 kHz
//! hump) still gets exactly one receiver on the right channel. The built-in FM raster is the US
//! one (200 kHz on odd tenths of a MHz); a plan overrides it for other regions.

use serde::{Deserialize, Serialize};

/// What makes a chain attach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Trigger {
    /// A track with a confirmed emitter candidate (C09/C10), detached when the track closes.
    #[default]
    ConfirmedTrack,
    /// The tuned window covers the spec's band (a known channel prior: pinned decoders whose
    /// signals are too short to detect at survey resolution, e.g. 1090 MHz squitters); detached
    /// when the window moves away or the stream ends.
    Coverage,
}

fn default_probe_s() -> f64 {
    0.5
}

/// One node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "kebab-case")]
pub enum NodeSpec {
    /// Pre-trigger SigMF recording `[trigger − pre_s, trigger + post_s)` (C25); skipped under a
    /// class that forbids content.
    Record {
        /// Pre-trigger, s.
        pre_s: f64,
        /// Post-trigger, s.
        post_s: f64,
    },
    /// C19 analog auto-mode over a collected window.
    AnalogAuto {
        /// Window start before the track's first sample, s.
        pre_s: f64,
        /// Window length, s.
        window_s: f64,
        /// Channel bandwidth requested from the receiver, Hz.
        bandwidth_hz: f64,
        /// Mode-selection probe window, s (0.5; 0 disables the probe).
        #[serde(default = "default_probe_s")]
        probe_s: f64,
        /// Modes that continue past the probe (`wfm`, `nbfm`, `am`, `ssb`, `cw`); empty = any.
        #[serde(default)]
        accept_modes: Vec<String>,
        /// The probe must find a 19 kHz pilot.
        #[serde(default)]
        require_pilot: bool,
    },
    /// C20 FSK bursts from the track's member boxes, then C21 framing.
    FskBursts {
        /// Signal-free pad either side of each box, s.
        pad_s: f64,
        /// Rolling sample buffer, s.
        retain_s: f64,
        /// Fewest bursts for framing inference.
        min_bursts: usize,
        /// Bursts after which the chain finishes.
        max_bursts: usize,
    },
    /// DDC to `output_rate_hz` around the spec band's centre (identity when it equals the input).
    Ddc {
        /// Output rate, Hz.
        output_rate_hz: f64,
        /// Flat bandwidth, Hz.
        bandwidth_hz: f64,
    },
    /// Subprocess decoder plugin.
    Plugin {
        /// Manifest path (relative to the repository root).
        manifest: String,
        /// Zero samples pushed at detach so block-buffered decoders flush their tail.
        #[serde(default)]
        tail_pad_samples: usize,
        /// Longest wait for decodes to settle at detach, s.
        #[serde(default)]
        settle_s: f64,
    },
}

fn one() -> u32 {
    1
}

/// A chain spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainSpec {
    /// Id.
    pub id: String,
    /// Trigger.
    #[serde(default)]
    pub trigger: Trigger,
    /// Candidate centre inside one of these `[lo, hi]` ranges, Hz (empty: any).
    #[serde(default)]
    pub freq_hz: Vec<[f64; 2]>,
    /// Candidate bandwidth in `[lo, hi]`, Hz.
    #[serde(default)]
    pub bandwidth_hz: Option<[f64; 2]>,
    /// Candidate burstiness must match.
    #[serde(default)]
    pub bursty: Option<bool>,
    /// Channel raster, Hz: candidates snap to it and one chain runs per channel.
    #[serde(default)]
    pub raster_hz: Option<f64>,
    /// Raster offset, Hz (channel centres at `k · raster + offset`).
    #[serde(default)]
    pub raster_offset_hz: f64,
    /// The track needs this many member detections before the chain attaches (1).
    #[serde(default = "one")]
    pub min_detections: u32,
    /// The chain writes content: refused unless the source class permits content.
    #[serde(default)]
    pub requires_content: bool,
    /// Nodes.
    pub nodes: Vec<NodeSpec>,
}

/// A validated node list.
#[derive(Clone, Debug, PartialEq)]
pub enum ChainShape {
    /// Analog auto-mode.
    Analog {
        /// Window start before the trigger box, s.
        pre_s: f64,
        /// Window, s.
        window_s: f64,
        /// Channel bandwidth, Hz.
        bandwidth_hz: f64,
        /// Probe, s.
        probe_s: f64,
        /// Accepted modes (lower-case names; empty = any).
        accept_modes: Vec<String>,
        /// Pilot required.
        require_pilot: bool,
    },
    /// FSK bursts + framing.
    Fsk {
        /// Pad, s.
        pad_s: f64,
        /// Buffer, s.
        retain_s: f64,
        /// Framing minimum.
        min_bursts: usize,
        /// Finish after.
        max_bursts: usize,
    },
    /// DDC + plugin.
    Plugin {
        /// DDC `(output_rate_hz, bandwidth_hz)`.
        ddc: Option<(f64, f64)>,
        /// Manifest path.
        manifest: String,
        /// Tail pad.
        tail_pad_samples: usize,
        /// Settle, s.
        settle_s: f64,
    },
}

impl ChainSpec {
    /// The recording node, if any.
    pub fn record(&self) -> Option<(f64, f64)> {
        self.nodes.iter().find_map(|n| match n {
            NodeSpec::Record { pre_s, post_s } => Some((*pre_s, *post_s)),
            _ => None,
        })
    }

    /// Validates the node list into its shape.
    pub fn shape(&self) -> Result<ChainShape, String> {
        let body: Vec<&NodeSpec> = self
            .nodes
            .iter()
            .filter(|n| !matches!(n, NodeSpec::Record { .. }))
            .collect();
        if self
            .nodes
            .iter()
            .filter(|n| matches!(n, NodeSpec::Record { .. }))
            .count()
            > 1
        {
            return Err("at most one record node".into());
        }
        if let Some((pre, post)) = self.record() {
            if !(pre >= 0.0 && post >= 0.0 && pre + post > 0.0) {
                return Err("record node needs pre_s, post_s >= 0 and a non-empty window".into());
            }
        }
        match body.as_slice() {
            [
                NodeSpec::AnalogAuto {
                    pre_s,
                    window_s,
                    bandwidth_hz,
                    probe_s,
                    accept_modes,
                    require_pilot,
                },
            ] => {
                if !self.requires_content {
                    return Err("analog-auto writes content: set requires_content".into());
                }
                if !(*window_s > 0.0 && *bandwidth_hz > 0.0 && *pre_s >= 0.0 && *probe_s >= 0.0) {
                    return Err("analog-auto needs window_s, bandwidth_hz > 0".into());
                }
                Ok(ChainShape::Analog {
                    pre_s: *pre_s,
                    window_s: *window_s,
                    bandwidth_hz: *bandwidth_hz,
                    probe_s: *probe_s,
                    accept_modes: accept_modes.iter().map(|m| m.to_lowercase()).collect(),
                    require_pilot: *require_pilot,
                })
            }
            [
                NodeSpec::FskBursts {
                    pad_s,
                    retain_s,
                    min_bursts,
                    max_bursts,
                },
            ] => {
                if !(*pad_s >= 0.0 && *retain_s > 0.0 && *max_bursts >= (*min_bursts).max(1)) {
                    return Err("fsk-bursts needs retain_s > 0 and max_bursts >= min_bursts".into());
                }
                Ok(ChainShape::Fsk {
                    pad_s: *pad_s,
                    retain_s: *retain_s,
                    min_bursts: *min_bursts,
                    max_bursts: *max_bursts,
                })
            }
            [
                rest @ ..,
                NodeSpec::Plugin {
                    manifest,
                    tail_pad_samples,
                    settle_s,
                },
            ] => {
                let ddc = match rest {
                    [] => None,
                    [
                        NodeSpec::Ddc {
                            output_rate_hz,
                            bandwidth_hz,
                        },
                    ] if *output_rate_hz > 0.0 && *bandwidth_hz > 0.0 => {
                        Some((*output_rate_hz, *bandwidth_hz))
                    }
                    _ => return Err("a plugin chain is [ddc] plugin".into()),
                };
                Ok(ChainShape::Plugin {
                    ddc,
                    manifest: manifest.clone(),
                    tail_pad_samples: *tail_pad_samples,
                    settle_s: *settle_s,
                })
            }
            _ => Err(
                "node list must be [record] analog-auto | [record] fsk-bursts | [record] [ddc] plugin"
                    .into(),
            ),
        }
    }

    /// Checks the spec.
    pub fn validate(&self) -> Result<(), String> {
        if self.trigger == Trigger::Coverage && self.freq_hz.is_empty() {
            return Err("a coverage chain needs freq_hz".into());
        }
        if self.freq_hz.iter().any(|r| r[0] >= r[1]) {
            return Err("freq_hz ranges must be [lo, hi] with lo < hi".into());
        }
        if self.raster_hz.is_some_and(|r| r <= 0.0) {
            return Err("raster_hz must be > 0".into());
        }
        self.shape().map(|_| ())
    }

    /// The candidate `[f_lo, f_hi]` with `bursty` matches this spec's priors.
    pub fn matches(&self, f_lo: f64, f_hi: f64, bursty: Option<bool>) -> bool {
        let fc = 0.5 * (f_lo + f_hi);
        let bw = f_hi - f_lo;
        (self.freq_hz.is_empty() || self.freq_hz.iter().any(|r| fc >= r[0] && fc <= r[1]))
            && self.bandwidth_hz.is_none_or(|r| bw >= r[0] && bw <= r[1])
            && self.bursty.is_none_or(|b| bursty == Some(b))
    }

    /// The channel a candidate occupies: snapped to the raster and widened to the analog channel
    /// bandwidth (or one raster step) when the spec has a raster; unchanged otherwise.
    pub fn channel(&self, f_lo: f64, f_hi: f64) -> (f64, f64) {
        let Some(raster) = self.raster_hz else {
            return (f_lo, f_hi);
        };
        let fc = 0.5 * (f_lo + f_hi);
        let k = ((fc - self.raster_offset_hz) / raster).round();
        let center = k * raster + self.raster_offset_hz;
        let width = match self.shape() {
            Ok(ChainShape::Analog { bandwidth_hz, .. }) => bandwidth_hz,
            _ => raster,
        };
        (center - width / 2.0, center + width / 2.0)
    }

    /// A coverage spec's band is inside the window `[center ± usable/2]`.
    pub fn covered_by(&self, center_hz: f64, usable_hz: f64) -> bool {
        self.freq_hz
            .iter()
            .any(|r| r[0] >= center_hz - usable_hz / 2.0 && r[1] <= center_hz + usable_hz / 2.0)
    }
}

/// The first `ConfirmedTrack` spec matching a candidate.
pub fn select_for_track(
    specs: &[ChainSpec],
    f_lo: f64,
    f_hi: f64,
    bursty: Option<bool>,
) -> Option<&ChainSpec> {
    specs
        .iter()
        .find(|s| s.trigger == Trigger::ConfirmedTrack && s.matches(f_lo, f_hi, bursty))
}

/// The built-in registry (JSON, like a plan override).
pub const BUILTIN_CHAINS: &str = r#"[
  {
    "id": "wfm-rds",
    "freq_hz": [[87.5e6, 108.0e6]],
    "bandwidth_hz": [2e3, 400e3],
    "raster_hz": 200e3,
    "raster_offset_hz": 100e3,
    "requires_content": true,
    "nodes": [
      { "node": "record", "pre_s": 0.25, "post_s": 0.25 },
      { "node": "analog-auto", "pre_s": 0.5, "window_s": 4.0, "bandwidth_hz": 200e3,
        "probe_s": 0.5, "accept_modes": ["wfm"], "require_pilot": true }
    ]
  },
  {
    "id": "adsb-readsb",
    "trigger": "coverage",
    "freq_hz": [[1089.0e6, 1091.0e6]],
    "nodes": [
      { "node": "ddc", "output_rate_hz": 2.4e6, "bandwidth_hz": 2.0e6 },
      { "node": "plugin", "manifest": "plugins/readsb/manifest.json",
        "tail_pad_samples": 131072, "settle_s": 5.0 }
    ]
  },
  {
    "id": "fsk-bursts",
    "bandwidth_hz": [2e3, 200e3],
    "bursty": true,
    "min_detections": 4,
    "nodes": [
      { "node": "fsk-bursts", "pad_s": 0.02, "retain_s": 3.0, "min_bursts": 4, "max_bursts": 256 }
    ]
  }
]"#;

/// The built-in registry.
pub fn builtin_chains() -> Vec<ChainSpec> {
    serde_json::from_str(BUILTIN_CHAINS).expect("built-in chain registry parses")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_validates_and_selects_by_priors() {
        let specs = builtin_chains();
        for s in &specs {
            s.validate().unwrap();
        }
        // A narrow fragment of an FM station, even one that looks bursty, is a WFM channel.
        let fm = select_for_track(&specs, 101.2055e6, 101.2195e6, Some(true)).unwrap();
        assert_eq!(fm.id, "wfm-rds");
        let (lo, hi) = fm.channel(101.2055e6, 101.2195e6);
        assert!(
            (lo - 101.2e6).abs() < 1.0 && (hi - 101.4e6).abs() < 1.0,
            "{lo} {hi}"
        );
        let fsk = select_for_track(&specs, 433.96e6, 433.99e6, Some(true)).unwrap();
        assert_eq!(fsk.id, "fsk-bursts");
        assert_eq!(fsk.min_detections, 4);
        assert_eq!(fsk.channel(433.96e6, 433.99e6), (433.96e6, 433.99e6));
        assert!(select_for_track(&specs, 446.0e6, 446.007e6, Some(false)).is_none());
        let adsb = specs.iter().find(|s| s.id == "adsb-readsb").unwrap();
        assert!(adsb.covered_by(1090e6, 2.2e6));
        assert!(!adsb.covered_by(1085e6, 2.2e6));
    }

    #[test]
    fn invalid_node_lists_are_rejected() {
        let bad: ChainSpec = serde_json::from_value(serde_json::json!({
            "id": "x", "nodes": [{ "node": "analog-auto", "pre_s": 0, "window_s": 1, "bandwidth_hz": 1e5 }]
        }))
        .unwrap();
        assert!(
            bad.validate().is_err(),
            "content chain without requires_content"
        );
        let bad: ChainSpec = serde_json::from_value(serde_json::json!({
            "id": "y", "nodes": [{ "node": "ddc", "output_rate_hz": 1e6, "bandwidth_hz": 1e6 }]
        }))
        .unwrap();
        assert!(bad.validate().is_err());
    }
}
