//! Chain specs: data, not code (ADR-0001 S1, ADR-0003 routing).
//!
//! A [`ChainSpec`] says *when* a runtime chain attaches (a trigger plus frequency, bandwidth,
//! burstiness, channel-raster and member-count priors) and *what* it runs (a node list). The
//! registry is JSON: the built-in one ([`BUILTIN_CHAINS`]) or `ScanPlan.extra.pipeline.chains`.
//! Adding a decoder is a manifest plus a spec entry; nothing is recompiled and capture never
//! restarts.
//!
//! Node lists are validated into one of four shapes ([`ChainShape`]):
//! - `[record?] analog-auto` — C19 auto mode (estimate → mode → WFM + RDS); writes content, so
//!   `requires_content` must be set. A short probe window runs mode selection first; the chain
//!   continues only when the mode is accepted (`accept_modes`, `require_pilot`);
//! - `[record?] fsk-bursts` — C13/C14 blind estimate, C20 demod per burst, C21 framing; content
//!   fails closed unless the emitter is classified;
//! - `[record?] ddc? plugin` — DDC to the plugin's rate, then a subprocess decoder (ADR-0003);
//! - `trunk-cc` — C23 control-channel hunt (T-287, [`crate::chains::trunk`]). **Metadata only**,
//!   and the validator enforces it: a `trunk-cc` spec that sets `requires_content` or carries a
//!   `record` node is refused, so the hunt can never become a content chain and the fail-closed
//!   class a 12.5 kHz LMR band derives (`metadata-only`) never has anything of its to refuse.
//!
//! **Channel priors.** A spec with `raster_hz` snaps a candidate's centre to the band's channel
//! raster and widens it to the node's channel bandwidth, and at most one chain runs per channel.
//! This is how a WFM station the detector covers with several narrow boxes (its noisy 200 kHz
//! hump) still gets exactly one receiver on the right channel.
//!
//! **FM region (T-037b).** The built-in `wfm-rds` band and raster follow [`FmRegion`]:
//! North America 87.5–108 MHz on a 200 kHz raster at odd tenths; ITU default (Europe, Africa,
//! Asia, Oceania, South America) 87.5–108 MHz on a 100 kHz raster; Japan 76–95 MHz on a 100 kHz
//! raster. [`FmRegion::from_site`] picks it from the configured site (no site: North America, the
//! previous default); a plan's `chains` still overrides the whole registry.

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
    /// The tuned window **overlaps** the spec's band, and the chain then decides for itself, from
    /// measured frequency-channel occupancy, whether there is anything to work on (T-287).
    ///
    /// This is the FCO-driven trigger C23 needs and the one [`Coverage`](Self::Coverage) cannot
    /// be: a hunt band is wider than any window (851–869 MHz against 20 MHz at best), so
    /// [`ChainSpec::covered_by`]'s containment never fires, and the band is a *prior about where
    /// control channels live*, never a frequency to tune to. The band decides only whether to look
    /// at all; which channels are worth a demodulation is decided by occupancy the chain measures
    /// over its own dwell, and confirmation by frame sync plus CRC after that.
    ///
    /// **Admission control** (the thing T-267 noted an FCO trigger would otherwise lack) is stated
    /// in [`NodeSpec::TrunkCc`] and bounded there; one chain per spec runs at a time.
    Occupancy,
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
        /// At detach the plugin's input ends (EOF) and the chain waits for it to exit; it is
        /// stopped after this long without progress, s (T-103; lossless replays allow at least
        /// 30 s).
        #[serde(default)]
        settle_s: f64,
    },
    /// C23 control-channel hunt over the tuned window's channel raster (T-287).
    ///
    /// Every field here is an **admission bound**, not a tuning knob: the detection thresholds are
    /// a priori and live in `hk_detect::trunk` (occupancy floor `MIN_CC_FCO`, sync tolerance, the
    /// sync and CRC counts), where a run cannot reach them. These four bound what one hunt is
    /// allowed to *spend*:
    ///
    /// - `window_s` — samples held per pass, so the chain's memory is one window, not a stream;
    /// - `period_s` — least stream time between passes, so a long dwell does not re-sweep
    ///   continuously;
    /// - `max_channels` — most raster channels measured for occupancy in one pass;
    /// - `max_demods` — most candidates demodulated in one pass. This is the bound that matters:
    ///   candidacy is cheap and demodulation is not, and on a busy LMR band many channels are
    ///   continuously occupied. Above the cap the highest-FCO candidates are taken (ties by
    ///   distance from the tuned centre), which is deterministic and blind.
    /// - `max_follows` — most **granted** channels followed in one pass (T-269). A control
    ///   channel can issue arbitrarily many grants in one window, and each followed one costs a
    ///   channelizer allocation; this bounds that. Refusals are counted, never silent.
    TrunkCc {
        /// Samples collected per hunt pass, s.
        window_s: f64,
        /// Most raster channels measured for occupancy in one pass.
        max_channels: usize,
        /// Most candidates demodulated in one pass.
        max_demods: usize,
        /// Least stream time between passes, s.
        period_s: f64,
        /// Most granted channels followed in one pass (T-269).
        #[serde(default = "default_max_follows")]
        max_follows: usize,
    },
}

fn one() -> u32 {
    1
}

/// Default [`NodeSpec::TrunkCc::max_follows`]: granted channels followed in one pass.
///
/// A spend bound, not a threshold. It matches `max_demods` because the work is the same order —
/// one channelizer allocation over the buffered window each — and a site with more than eight
/// voice channels live inside one ≤20 MHz window at the same instant is past what a single
/// half-duplex front end can honestly follow anyway. What it refuses is counted
/// (`cc_follow_refused`), so a busier site shows up as a number rather than as silence.
fn default_max_follows() -> usize {
    8
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
    /// C23 control-channel hunt (T-287) and grant following (T-269). Metadata only.
    TrunkCc {
        /// Samples per pass, s.
        window_s: f64,
        /// Most raster channels measured per pass.
        max_channels: usize,
        /// Most candidates demodulated per pass.
        max_demods: usize,
        /// Least stream time between passes, s.
        period_s: f64,
        /// Most granted channels followed per pass.
        max_follows: usize,
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
                NodeSpec::TrunkCc {
                    window_s,
                    max_channels,
                    max_demods,
                    period_s,
                    max_follows,
                },
            ] => {
                // The class gate, resolved structurally rather than worked around. A 12.5 kHz LMR
                // band derives no positive content prior, so `class::band_class` falls through to
                // the fail-closed `metadata-only`, and under gating that class refuses every chain
                // with `requires_content` and every recording. The hunt is unaffected because what
                // it writes — that a control channel exists at a frequency — is metadata, which
                // `hk_model::content` says is never gated. Making that a validation rule rather
                // than a convention means no spec can quietly turn the hunt into a content chain.
                if self.requires_content {
                    return Err(
                        "trunk-cc is metadata-only: it must not set requires_content".into(),
                    );
                }
                if self.record().is_some() {
                    return Err("trunk-cc is metadata-only: it must not carry a record node".into());
                }
                if !(*window_s > 0.0 && *period_s >= 0.0) {
                    return Err("trunk-cc needs window_s > 0 and period_s >= 0".into());
                }
                if *max_channels == 0 || *max_demods == 0 || *max_follows == 0 {
                    return Err("trunk-cc needs max_channels, max_demods, max_follows >= 1".into());
                }
                Ok(ChainShape::TrunkCc {
                    window_s: *window_s,
                    max_channels: *max_channels,
                    max_demods: *max_demods,
                    period_s: *period_s,
                    max_follows: *max_follows,
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
                "node list must be [record] analog-auto | [record] fsk-bursts | \
                 [record] [ddc] plugin | trunk-cc"
                    .into(),
            ),
        }
    }

    /// Checks the spec.
    pub fn validate(&self) -> Result<(), String> {
        if self.trigger == Trigger::Coverage && self.freq_hz.is_empty() {
            return Err("a coverage chain needs freq_hz".into());
        }
        if self.trigger == Trigger::Occupancy
            && (self.freq_hz.is_empty() || self.raster_hz.is_none())
        {
            return Err("an occupancy chain needs freq_hz and raster_hz".into());
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

    /// An [`Trigger::Occupancy`] spec's band **overlaps** the window `[center ± usable/2]`.
    ///
    /// The opposite containment to [`covered_by`](Self::covered_by), and deliberately so: a hunt
    /// band is wider than any window the radio can hold at once, so containment would never fire.
    pub fn overlaps_window(&self, center_hz: f64, usable_hz: f64) -> bool {
        let (lo, hi) = (center_hz - usable_hz / 2.0, center_hz + usable_hz / 2.0);
        self.freq_hz.iter().any(|r| r[0] < hi && r[1] > lo)
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
    "id": "trunk-cc-hunt",
    "trigger": "occupancy",
    "freq_hz": [[450.0e6, 470.0e6], [769.0e6, 775.0e6], [799.0e6, 805.0e6],
                [851.0e6, 869.0e6], [935.0e6, 940.0e6]],
    "raster_hz": 12.5e3,
    "nodes": [
      { "node": "trunk-cc", "window_s": 0.5, "max_channels": 64, "max_demods": 8,
        "period_s": 10.0, "max_follows": 8 }
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

/// The FM broadcast channel plan the built-in `wfm-rds` spec uses (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FmRegion {
    /// US, Canada, Mexico, Central America, Caribbean: 87.5–108 MHz, 200 kHz raster on odd
    /// tenths of a MHz.
    NorthAmerica,
    /// ITU default: 87.5–108 MHz, 100 kHz raster (a superset of the 200 kHz odd-tenth plan).
    Itu,
    /// Japan: 76–95 MHz, 100 kHz raster.
    Japan,
}

impl FmRegion {
    /// The region of a site `[lat, lon]`, degrees. Coarse boxes (unverified at borders):
    /// Japan 24–46° N, 122–154° E; North America 7–75° N, 170–50° W; anywhere else the ITU
    /// default. No site: North America.
    pub fn from_site(site: Option<[f64; 2]>) -> Self {
        let Some([lat, lon]) = site else {
            return FmRegion::NorthAmerica;
        };
        if (24.0..=46.0).contains(&lat) && (122.0..=154.0).contains(&lon) {
            FmRegion::Japan
        } else if (7.0..=75.0).contains(&lat) && (-170.0..=-50.0).contains(&lon) {
            FmRegion::NorthAmerica
        } else {
            FmRegion::Itu
        }
    }

    /// `(band [lo, hi] Hz, raster Hz, raster offset Hz)`.
    pub fn plan(self) -> ([f64; 2], f64, f64) {
        match self {
            FmRegion::NorthAmerica => ([87.5e6, 108.0e6], 200e3, 100e3),
            FmRegion::Itu => ([87.5e6, 108.0e6], 100e3, 0.0),
            FmRegion::Japan => ([76.0e6, 95.0e6], 100e3, 0.0),
        }
    }
}

/// The built-in registry with `wfm-rds` on `region`'s band and raster.
pub fn builtin_chains_for(region: FmRegion) -> Vec<ChainSpec> {
    let mut specs = builtin_chains();
    if let Some(s) = specs.iter_mut().find(|s| s.id == "wfm-rds") {
        let (band, raster, offset) = region.plan();
        s.freq_hz = vec![band];
        s.raster_hz = Some(raster);
        s.raster_offset_hz = offset;
    }
    specs
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
    fn fm_raster_follows_the_region() {
        assert_eq!(FmRegion::from_site(None), FmRegion::NorthAmerica);
        assert_eq!(
            FmRegion::from_site(Some([40.7, -74.0])),
            FmRegion::NorthAmerica
        );
        assert_eq!(FmRegion::from_site(Some([52.2, 0.12])), FmRegion::Itu);
        assert_eq!(FmRegion::from_site(Some([-23.5, -46.6])), FmRegion::Itu);
        assert_eq!(FmRegion::from_site(Some([35.7, 139.7])), FmRegion::Japan);
        assert_eq!(
            builtin_chains_for(FmRegion::NorthAmerica),
            builtin_chains(),
            "North America is the previous built-in plan"
        );
        let wfm = |r| {
            builtin_chains_for(r)
                .into_iter()
                .find(|s| s.id == "wfm-rds")
                .unwrap()
        };
        // Europe: 100 kHz raster, so 94.25 MHz fragments snap to 94.2 or 94.3 MHz.
        let eu = wfm(FmRegion::Itu);
        eu.validate().unwrap();
        let (lo, hi) = eu.channel(94.28e6, 94.30e6);
        assert!((0.5 * (lo + hi) - 94.3e6).abs() < 1.0, "{lo} {hi}");
        let (lo, hi) = eu.channel(94.21e6, 94.23e6);
        assert!((0.5 * (lo + hi) - 94.2e6).abs() < 1.0);
        let (lo, hi) = wfm(FmRegion::NorthAmerica).channel(94.28e6, 94.30e6);
        assert!((0.5 * (lo + hi) - 94.3e6).abs() < 1.0);
        let jp = wfm(FmRegion::Japan);
        assert!(jp.matches(80.0e6, 80.1e6, Some(false)));
        assert!(!jp.matches(100.0e6, 100.1e6, Some(false)));
    }

    /// T-287: the hunt is in the **built-in** registry, so a normal run carries it, and the
    /// contract refuses to let it become a content chain.
    #[test]
    fn the_trunk_cc_hunt_is_built_in_metadata_only_and_band_gated() {
        let specs = builtin_chains();
        let hunt = specs
            .iter()
            .find(|s| s.id == "trunk-cc-hunt")
            .expect("the hunt ships in the built-in registry, not only in a hand-written plan");
        hunt.validate().unwrap();
        assert_eq!(hunt.trigger, Trigger::Occupancy);
        assert!(!hunt.requires_content, "the hunt writes metadata only");
        assert!(hunt.record().is_none());
        assert_eq!(hunt.raster_hz, Some(12_500.0));
        assert!(matches!(hunt.shape(), Ok(ChainShape::TrunkCc { .. })));
        // 800 MHz public safety. The band is 18 MHz wide and the window is 500 kHz, so
        // `covered_by`'s containment can never fire — which is the whole reason the trigger
        // exists rather than reusing `coverage`.
        assert!(hunt.overlaps_window(851.0125e6, 500e3));
        assert!(!hunt.covered_by(851.0125e6, 500e3));
        // A window straddling the lower edge still counts: the hunt sweeps what it can see.
        assert!(hunt.overlaps_window(850.9e6, 500e3));
        // Bands where nothing trunked lives are left alone, so an FM, 433 MHz or 1090 MHz run is
        // not made to carry a hunt it would only ever find nothing in.
        assert!(!hunt.overlaps_window(100e6, 2e6));
        assert!(!hunt.overlaps_window(433.92e6, 2e6));
        assert!(!hunt.overlaps_window(1090e6, 2.4e6));
        assert!(!hunt.overlaps_window(152.36e6, 2e6));
    }

    #[test]
    fn a_trunk_cc_spec_may_never_become_a_content_chain() {
        let node = serde_json::json!({ "node": "trunk-cc", "window_s": 0.5,
            "max_channels": 8, "max_demods": 2, "period_s": 1.0 });
        let spec = |patch: serde_json::Value| -> ChainSpec {
            let mut v = serde_json::json!({ "id": "t", "trigger": "occupancy",
                "freq_hz": [[851e6, 869e6]], "raster_hz": 12500.0, "nodes": [node.clone()] });
            for (k, val) in patch.as_object().unwrap() {
                v[k.as_str()] = val.clone();
            }
            serde_json::from_value(v).unwrap()
        };
        spec(serde_json::json!({})).validate().unwrap();
        assert!(
            spec(serde_json::json!({ "requires_content": true }))
                .validate()
                .is_err(),
            "the hunt is metadata-only by contract, not by convention"
        );
        assert!(
            spec(serde_json::json!({ "nodes": [
                { "node": "record", "pre_s": 0.1, "post_s": 0.1 }, node.clone()] }))
            .validate()
            .is_err(),
            "a record node writes IQ, which is content"
        );
        // An occupancy chain without a raster has nothing to sweep.
        assert!(
            spec(serde_json::json!({ "raster_hz": null }))
                .validate()
                .is_err()
        );
        assert!(
            spec(serde_json::json!({ "freq_hz": [] }))
                .validate()
                .is_err(),
            "a hunt with no band prior would hunt everywhere"
        );
        // A follow cap is optional in a spec (T-269 added it) and defaults to something that
        // actually follows: an old plan must not silently turn the follower off.
        assert!(matches!(
            spec(serde_json::json!({})).shape(),
            Ok(ChainShape::TrunkCc { max_follows, .. }) if max_follows >= 1
        ));
        assert!(
            spec(
                serde_json::json!({ "nodes": [{ "node": "trunk-cc", "window_s": 0.5,
                "max_channels": 8, "max_demods": 2, "period_s": 1.0, "max_follows": 0 }] })
            )
            .validate()
            .is_err(),
            "a follow cap of zero would disable following without saying so"
        );
        // Admission bounds must actually bound.
        assert!(
            spec(
                serde_json::json!({ "nodes": [{ "node": "trunk-cc", "window_s": 0.5,
                "max_channels": 0, "max_demods": 2, "period_s": 1.0 }] })
            )
            .validate()
            .is_err()
        );
        assert!(
            spec(
                serde_json::json!({ "nodes": [{ "node": "trunk-cc", "window_s": 0.0,
                "max_channels": 8, "max_demods": 2, "period_s": 1.0 }] })
            )
            .validate()
            .is_err()
        );
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
