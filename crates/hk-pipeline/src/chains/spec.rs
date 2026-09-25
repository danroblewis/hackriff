//! Chain specs: data, not code (ADR-0001 S1, ADR-0003 routing).
//!
//! A [`ChainSpec`] says *when* a runtime chain attaches (a trigger plus frequency, bandwidth,
//! burstiness, channel-raster and member-count priors) and *what* it runs (a node list). The
//! registry is JSON: the built-in one ([`BUILTIN_CHAINS`]) or `ScanPlan.extra.pipeline.chains`.
//! Adding a decoder is a manifest plus a spec entry; nothing is recompiled and capture never
//! restarts.
//!
//! Node lists are validated into one of seven shapes ([`ChainShape`]):
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
//! - `sweep-char` — sweep characterisation of a candidate region from its IQ (T-297,
//!   [`crate::chains::sweep`]). **Metadata only**, enforced the same way, and on
//!   [`Trigger::EveryTrack`]: it attaches *beside* the decode chain rather than instead of it.
//! - `classify` — the C15 classifier over a candidate region's own IQ (T-878,
//!   [`crate::chains::classify`]). **Metadata only**, enforced the same way, and the other shape on
//!   [`Trigger::EveryTrack`]: the classifier runs for every confirmed track whether or not any
//!   decode chain matched it, attached or succeeded (ADR-0016 §4, "Placement").
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
    /// **The band is a dwell-budget gate, not a search prior (T-615).** Nothing inside the hunt
    /// reads it: the raster origin is the tuned centre, candidacy is measured occupancy against
    /// the window's own floor, and confirmation is frame sync plus CRC — so the same hunt finds a
    /// continuous four-level control channel at any frequency the window holds (asserted by
    /// `tests/e2e/tests/acceptance/signal_085.rs` at 300 MHz, outside every LMR allocation). What
    /// the band decides is only whether to *spend* a pass: each one holds `window_s × fs` samples
    /// and runs up to `max_demods` down-conversions plus C4FM demodulations, and outside the
    /// land-mobile allocations every continuously-occupied raster channel (a WFM station fills
    /// sixteen, a DTV channel ~480) reaches candidacy and costs a demodulation that sync + CRC
    /// then rejects. The built-in registry therefore gates on the LMR allocations `SIGNAL-085`
    /// names (VHF, UHF, 700, 800 and 900 MHz); a plan that wants the hunt everywhere sets
    /// `freq_hz: [[1e6, 6e9]]` and pays for it, with no other change.
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
    /// A confirmed track, **in addition to** the one decode chain [`select_for_track`] chose for
    /// it (T-297).
    ///
    /// [`ConfirmedTrack`](Self::ConfirmedTrack) is a *selection*: the first matching spec wins and
    /// the rest never run, which is right for decoding (one receiver per emission) and wrong for
    /// measuring. A measurement is not in competition with a decode — it asks a different question
    /// of the same region — and the region that most needs measuring is precisely the one no
    /// decoder claimed. Measured on T-255's SF9 scene: the swept region matches `fsk-bursts`, never
    /// reaches its four member detections, and closes `unmatched` with **no chain at all**, so a
    /// spec ordered after `fsk-bursts` would never be reached for it and one ordered before would
    /// take every bursty track away from it.
    ///
    /// A chain on this trigger must therefore be cheap and must write only evidence. Its admission
    /// is stated in its node spec and bounded there ([`NodeSpec::SweepChar`]), and the manager caps
    /// how many run at once.
    EveryTrack,
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
    /// Sweep characterisation of a candidate region from its IQ (T-297,
    /// [`crate::chains::sweep`]).
    ///
    /// Every field here is an **admission bound**, not a tuning knob: what decides whether a region
    /// sweeps lives in `hk_dsp::chirp` with the measurement behind it (`SWEEP_MIN_PAPR`,
    /// `SWEEP_MAX_DISAGREEMENT`), where a run cannot reach it. These bound what one
    /// characterisation is allowed to *spend*:
    ///
    /// - `window_s` — samples held per pass, so the chain's memory is one window, not a stream;
    /// - `frame_s` — the analysis frame the two-lag test runs on (T-294 measured 4.096 ms);
    /// - `max_passes` — most windows examined before the chain gives up on this region. The chain
    ///   also stops at the **first** characterisation, so this bounds the miss case, not the hit;
    /// - `max_chains` — most characterising chains alive at once across the run. This is the bound
    ///   that matters: the trigger is per confirmed track, and a busy band has many.
    SweepChar {
        /// Samples collected per pass, s.
        window_s: f64,
        /// Analysis frame the sweep test runs on, s.
        frame_s: f64,
        /// Most windows examined per region.
        max_passes: u64,
        /// Most characterising chains alive at once.
        max_chains: usize,
    },
    /// The C15 classifier over a candidate region's own IQ (T-878, [`crate::chains::classify`]).
    ///
    /// Like [`NodeSpec::SweepChar`], every field is an **admission bound**: what the classifier
    /// decides is `hk_classify`'s (its densities, gates and thresholds), which no spec can reach.
    /// These bound what one classification may *spend*:
    ///
    /// - `pad_s` — signal-free pad either side of the analysed box, as `fsk-bursts` takes it;
    /// - `retain_s` — the rolling sample buffer, so the chain's memory is bounded (and capped
    ///   again at the same sample ceiling the fsk chain uses, whatever the rate);
    /// - `window_s` — the longest extent analysed. A burst shorter than this is analysed whole; a
    ///   continuous emission is analysed over its first `window_s`, and is classified as soon as
    ///   that much of it has arrived rather than when its track closes;
    /// - `max_chains` — most classifying chains alive at once across the run: the trigger is per
    ///   confirmed track, and a busy band has many.
    Classify {
        /// Pad either side of the analysed box, s.
        pad_s: f64,
        /// Rolling sample buffer, s.
        retain_s: f64,
        /// Longest extent analysed, s.
        window_s: f64,
        /// Most classifying chains alive at once.
        max_chains: usize,
    },
    /// The narrowband-FSK **frame hunt** over a candidate region's own IQ (T-950,
    /// [`crate::chains::frames`]): channelise the track, then try each framing in the catalogue —
    /// FLEX today — and keep what frame-syncs and BCH-checks. Which decoder a signal gets is
    /// decided by sync plus check on its own symbols, never by where it was found.
    ///
    /// Every field is an admission bound:
    ///
    /// - `pad_s` — pad either side of a transmission, as `fsk-bursts` takes it;
    /// - `retain_s` — the channelised buffer (held at the ~80 kSps channel rate, so its memory does not grow with the device rate) — must cover a segment plus the settle wait;
    /// - `segment_s` — longest stretch decoded at once. A continuous transmitter is decoded in
    ///   segments of this length, overlapping by one frame, so memory and latency stay bounded;
    /// - `max_chains` — most hunting chains alive at once across the run.
    FskFrames {
        /// Pad either side of a transmission, s.
        pad_s: f64,
        /// Rolling sample buffer, s.
        retain_s: f64,
        /// Longest stretch decoded at once, s.
        segment_s: f64,
        /// Most hunting chains alive at once.
        max_chains: usize,
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
    /// Sweep characterisation (T-297). Metadata only.
    Sweep {
        /// Samples per pass, s.
        window_s: f64,
        /// Analysis frame, s.
        frame_s: f64,
        /// Most windows examined per region.
        max_passes: u64,
        /// Most characterising chains alive at once.
        max_chains: usize,
    },
    /// The C15 classifier (T-878). Metadata only.
    Classify {
        /// Pad, s.
        pad_s: f64,
        /// Buffer, s.
        retain_s: f64,
        /// Longest extent analysed, s.
        window_s: f64,
        /// Most classifying chains alive at once.
        max_chains: usize,
    },
    /// The narrowband-FSK frame hunt (T-950).
    FskFrames {
        /// Pad, s.
        pad_s: f64,
        /// Buffer, s.
        retain_s: f64,
        /// Longest stretch decoded at once, s.
        segment_s: f64,
        /// Most hunting chains alive at once.
        max_chains: usize,
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
                NodeSpec::SweepChar {
                    window_s,
                    frame_s,
                    max_passes,
                    max_chains,
                },
            ] => {
                // Metadata only, enforced structurally for the same reason `trunk-cc` is: what a
                // characterisation writes is a measured number about a region, which
                // `hk_model::content` says is never gated. A spec must not be able to quietly turn
                // a measuring chain into one that records or demodulates.
                if self.requires_content {
                    return Err(
                        "sweep-char is metadata-only: it must not set requires_content".into(),
                    );
                }
                if self.record().is_some() {
                    return Err(
                        "sweep-char is metadata-only: it must not carry a record node".into(),
                    );
                }
                if !(*window_s > 0.0 && *frame_s > 0.0 && *frame_s <= *window_s) {
                    return Err("sweep-char needs 0 < frame_s <= window_s".into());
                }
                if *max_passes == 0 || *max_chains == 0 {
                    return Err("sweep-char needs max_passes, max_chains >= 1".into());
                }
                Ok(ChainShape::Sweep {
                    window_s: *window_s,
                    frame_s: *frame_s,
                    max_passes: *max_passes,
                    max_chains: *max_chains,
                })
            }
            [
                NodeSpec::Classify {
                    pad_s,
                    retain_s,
                    window_s,
                    max_chains,
                },
            ] => {
                // Metadata only, for the reason `sweep-char` is: a classification is a measured
                // posterior about a region, which `hk_model::content` says is never gated.
                if self.requires_content {
                    return Err(
                        "classify is metadata-only: it must not set requires_content".into(),
                    );
                }
                if self.record().is_some() {
                    return Err("classify is metadata-only: it must not carry a record node".into());
                }
                if !(*pad_s >= 0.0 && *window_s > 0.0 && *retain_s >= *window_s + 2.0 * *pad_s) {
                    return Err(
                        "classify needs pad_s >= 0, window_s > 0 and retain_s >= window_s + 2 pad_s"
                            .into(),
                    );
                }
                if *max_chains == 0 {
                    return Err("classify needs max_chains >= 1".into());
                }
                Ok(ChainShape::Classify {
                    pad_s: *pad_s,
                    retain_s: *retain_s,
                    window_s: *window_s,
                    max_chains: *max_chains,
                })
            }
            [
                NodeSpec::FskFrames {
                    pad_s,
                    retain_s,
                    segment_s,
                    max_chains,
                },
            ] => {
                // Content is decided per decode by the emitter's class, as `fsk-bursts` decides
                // it; the chain itself records nothing, so a record node is refused.
                if self.record().is_some() {
                    return Err("fsk-frames must not carry a record node".into());
                }
                // A FLEX frame is 1.875 s; a segment must hold one plus its overlap.
                if !(*pad_s >= 0.0 && *segment_s >= 4.0 && *retain_s >= *segment_s + 2.0 * *pad_s) {
                    return Err(
                        "fsk-frames needs pad_s >= 0, segment_s >= 4 and retain_s >= segment_s + \
                         2 pad_s"
                            .into(),
                    );
                }
                if *max_chains == 0 {
                    return Err("fsk-frames needs max_chains >= 1".into());
                }
                Ok(ChainShape::FskFrames {
                    pad_s: *pad_s,
                    retain_s: *retain_s,
                    segment_s: *segment_s,
                    max_chains: *max_chains,
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
                 [record] [ddc] plugin | trunk-cc | sweep-char | classify | fsk-frames"
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
    "freq_hz": [[150.8e6, 174.0e6], [450.0e6, 470.0e6], [769.0e6, 775.0e6],
                [799.0e6, 805.0e6], [851.0e6, 869.0e6], [935.0e6, 940.0e6]],
    "raster_hz": 12.5e3,
    "nodes": [
      { "node": "trunk-cc", "window_s": 0.5, "max_channels": 64, "max_demods": 8,
        "period_s": 10.0, "max_follows": 8 }
    ]
  },
  {
    "id": "sweep-char",
    "trigger": "every-track",
    "bandwidth_hz": [2e3, 2e6],
    "nodes": [
      { "node": "sweep-char", "window_s": 0.2, "frame_s": 0.004096, "max_passes": 2,
        "max_chains": 4 }
    ]
  },
  {
    "id": "classify",
    "trigger": "every-track",
    "bandwidth_hz": [500, 2e6],
    "nodes": [
      { "node": "classify", "pad_s": 0.02, "retain_s": 3.0, "window_s": 0.25, "max_chains": 4 }
    ]
  },
  {
    "id": "fsk-frames",
    "trigger": "every-track",
    "bandwidth_hz": [4e3, 60e3],
    "nodes": [
      { "node": "fsk-frames", "pad_s": 0.05, "retain_s": 6.5, "segment_s": 4.0, "max_chains": 8 }
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
    fn the_trunk_cc_hunt_is_built_in_metadata_only_and_budget_gated_on_every_lmr_band() {
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
        // T-615: every LMR allocation SIGNAL-085 names is gated in, VHF included — before T-615
        // the VHF high band was missing, so a VHF trunked system never got a hunt at all.
        for f in [
            155.0125e6, 460.0125e6, 770.0125e6, 800.0125e6, 860.0125e6, 937.0125e6,
        ] {
            assert!(hunt.overlaps_window(f, 500e3), "{} MHz", f / 1e6);
        }
        // Outside them the gate is a dwell budget, not a statement that nothing could be found
        // there: an FM, 433 MHz or 1090 MHz run is not made to spend demodulations on channels
        // sync + CRC would only reject. A plan widens the band to hunt everywhere.
        assert!(!hunt.overlaps_window(100e6, 2e6));
        assert!(!hunt.overlaps_window(300e6, 2e6));
        assert!(!hunt.overlaps_window(433.92e6, 2e6));
        assert!(!hunt.overlaps_window(1090e6, 2.4e6));
        let mut anywhere = hunt.clone();
        anywhere.freq_hz = vec![[1e6, 6e9]];
        anywhere.validate().unwrap();
        for f in [100e6, 300e6, 433.92e6, 1090e6] {
            assert!(anywhere.overlaps_window(f, 2e6), "{} MHz", f / 1e6);
        }
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

    /// T-297: the characteriser ships in the **built-in** registry, so a normal run carries it,
    /// and it attaches *beside* a decode chain rather than competing with one.
    #[test]
    fn the_sweep_characteriser_is_built_in_and_never_competes_with_a_decode_chain() {
        let specs = builtin_chains();
        let sweep = specs
            .iter()
            .find(|s| s.id == "sweep-char")
            .expect("the characteriser ships in the built-in registry, not only in a plan");
        sweep.validate().unwrap();
        assert_eq!(sweep.trigger, Trigger::EveryTrack);
        assert!(!sweep.requires_content, "it measures and writes evidence");
        assert!(sweep.record().is_none());
        assert!(matches!(sweep.shape(), Ok(ChainShape::Sweep { .. })));

        // The whole point of the trigger: selection is untouched, so no existing run's choice of
        // decode chain changes. `select_for_track` only ever considers `ConfirmedTrack` specs.
        assert!(
            !specs
                .iter()
                .any(|s| s.trigger == Trigger::ConfirmedTrack && s.id == "sweep-char")
        );
        assert_eq!(
            select_for_track(&specs, 433.96e6, 433.99e6, Some(true))
                .unwrap()
                .id,
            "fsk-bursts"
        );
        assert_eq!(
            select_for_track(&specs, 101.2055e6, 101.2195e6, Some(true))
                .unwrap()
                .id,
            "wfm-rds"
        );

        // The region T-255 measures and no decode chain reaches: `fsk-bursts` matches it but needs
        // four member detections, and the swept track closes with three. The characteriser's
        // priors must cover it, or the capability still has no caller where it is needed most.
        assert!(sweep.matches(903.0347e6, 903.1617e6, Some(true)));
        // And the 2-FSK burst beside it, so the run has a control that is examined and declined.
        assert!(sweep.matches(902.9267e6, 902.9534e6, Some(true)));
    }

    /// T-878: the classifier ships in the built-in registry on the every-track trigger, so it runs
    /// beside whatever decode chain a track selects — and where none matched — never instead of
    /// one, and it can never be turned into a content chain.
    #[test]
    fn the_classifier_is_built_in_beside_every_decode_chain_and_metadata_only() {
        let specs = builtin_chains();
        let classify = specs
            .iter()
            .find(|s| s.id == "classify")
            .expect("the classifier ships in the built-in registry, not only in a plan");
        classify.validate().unwrap();
        assert_eq!(classify.trigger, Trigger::EveryTrack);
        assert!(!classify.requires_content);
        assert!(classify.record().is_none());
        assert!(matches!(
            classify.shape(),
            Ok(ChainShape::Classify { max_chains, .. }) if max_chains >= 1
        ));
        // Selection is untouched.
        assert_eq!(
            select_for_track(&specs, 433.96e6, 433.99e6, Some(true))
                .unwrap()
                .id,
            "fsk-bursts"
        );
        // The regions T-852 found unclassified: a POCSAG channel no decoder matches, a WFM station
        // only the FM chain takes, and a LoRa burst the fsk chain gives up on.
        assert!(classify.matches(152.355e6, 152.365e6, Some(true)));
        assert!(classify.matches(101.1e6, 101.3e6, Some(false)));
        assert!(classify.matches(903.0375e6, 903.1625e6, Some(true)));

        let node = |patch: serde_json::Value| -> ChainSpec {
            let mut n = serde_json::json!({ "node": "classify", "pad_s": 0.02, "retain_s": 3.0,
                "window_s": 0.25, "max_chains": 4 });
            for (k, v) in patch.as_object().unwrap() {
                n[k.as_str()] = v.clone();
            }
            serde_json::from_value(serde_json::json!({ "id": "c", "trigger": "every-track",
                "nodes": [n] }))
            .unwrap()
        };
        node(serde_json::json!({})).validate().unwrap();
        for bad in [
            serde_json::json!({ "max_chains": 0 }),
            serde_json::json!({ "window_s": 0.0 }),
            // The buffer must hold a whole window and its pads, or a continuous emission's box has
            // left it before it spans the window.
            serde_json::json!({ "retain_s": 0.2 }),
        ] {
            assert!(node(bad.clone()).validate().is_err(), "{bad} is unbounded");
        }
        let mut content = node(serde_json::json!({}));
        content.requires_content = true;
        assert!(content.validate().is_err(), "a posterior is not content");
    }

    #[test]
    fn a_sweep_char_spec_may_never_become_a_content_chain() {
        let node = serde_json::json!({ "node": "sweep-char", "window_s": 0.2,
            "frame_s": 0.004096, "max_passes": 2, "max_chains": 4 });
        let spec = |patch: serde_json::Value| -> ChainSpec {
            let mut v = serde_json::json!({ "id": "s", "trigger": "every-track",
                "nodes": [node.clone()] });
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
            "a characterisation is a measured number, not content"
        );
        assert!(
            spec(serde_json::json!({ "nodes": [
                { "node": "record", "pre_s": 0.1, "post_s": 0.1 }, node.clone()] }))
            .validate()
            .is_err(),
            "a record node writes IQ, which is content"
        );
        // The admission bounds must actually bound.
        for bad in [
            serde_json::json!({ "node": "sweep-char", "window_s": 0.2, "frame_s": 0.004096,
                "max_passes": 0, "max_chains": 4 }),
            serde_json::json!({ "node": "sweep-char", "window_s": 0.2, "frame_s": 0.004096,
                "max_passes": 2, "max_chains": 0 }),
            // A frame longer than the window can never be filled.
            serde_json::json!({ "node": "sweep-char", "window_s": 0.002, "frame_s": 0.004096,
                "max_passes": 2, "max_chains": 4 }),
        ] {
            assert!(
                spec(serde_json::json!({ "nodes": [bad] }))
                    .validate()
                    .is_err(),
                "an unbounded characteriser is not admissible"
            );
        }
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
