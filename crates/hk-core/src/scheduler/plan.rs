//! ScanPlan compilation: regions → a discovery pass of window-sized hops inside the source's
//! capabilities, plus the timing budget that keeps revisit targets.

use hk_model::{FreqRange, GainTableEntry, ScanPlan, ScanPlanId, ScanPolicy, Schedule};

use super::config::SchedulerConfig;
use crate::source::{BasebandFilters, Gains, SampleRates, SourceCapabilities};

/// Cuts (RF-path boundaries, gain-table edges) closer than this to a band edge are ignored, Hz.
const EDGE_HZ: f64 = 1.0;

/// Why a plan cannot be scheduled.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum PlanError {
    /// The plan has no regions.
    #[error("the scan plan has no regions")]
    Empty,
    /// A region is malformed.
    #[error("region {index}: {reason}")]
    InvalidRegion {
        /// Region index.
        index: usize,
        /// What is wrong.
        reason: String,
    },
    /// No region overlaps the source's frequency ranges.
    #[error("no region lies inside the source's frequency ranges")]
    NothingCoverable,
    /// A scheduler setting is invalid.
    #[error("invalid scheduler setting: {0}")]
    InvalidConfig(String),
    /// A gain setting is outside the source's gain stages.
    #[error("gain setting{}: {reason}", index.map_or(String::new(), |i| format!(" (gain table entry {i})")))]
    InvalidGain {
        /// Gain-table entry, or `None` for a default or user-intent gain.
        index: Option<usize>,
        /// What is wrong.
        reason: String,
    },
    /// The plan's schedule kind is not implemented yet.
    #[error("schedule kind {0} is not implemented by the v1 scheduler")]
    UnsupportedSchedule(&'static str),
    /// A plan update does not supersede the running version.
    #[error("plan {id} version {version} does not supersede running version {running}")]
    StaleVersion {
        /// Plan id.
        id: ScanPlanId,
        /// Offered version.
        version: u32,
        /// Running version.
        running: u32,
    },
}

/// What a discovery hop does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HopKind {
    /// A short sweep hop (sweep-only and sweep-then-dwell regions).
    Sweep,
    /// A long window of a dwell-only region.
    RegionDwell,
}

/// One window of the discovery pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hop {
    /// Sweep hop or dwell-only region window.
    pub kind: HopKind,
    /// Tuned centre on even passes, Hz.
    pub center_hz: f64,
    /// T-173: offset of the odd-pass centre from `center_hz`, Hz (± the plan's DC dither, or 0
    /// when neither direction stays on the hop's RF path and inside the source's range). See
    /// [`Hop::center_on_pass`].
    pub dither_hz: f64,
    /// Sample rate, Hz.
    pub rate_hz: f64,
    /// Baseband filter, Hz.
    pub baseband_filter_hz: Option<f64>,
    /// The slice of the plan this hop is responsible for; inside the usable span around the
    /// even-pass centre (and the odd-pass one when the slice leaves room for the dither). Hops of
    /// one kind tile the merged regions without overlap.
    pub covers: FreqRange,
    /// Gains.
    pub gains: Gains,
    /// Gain-table entry that set the gains.
    pub gain_entry: Option<u16>,
    /// The gain-table entry names an accessory/filter port (trust pending T-028).
    pub accessory: bool,
    /// RF path of the centre.
    pub rf_path: u8,
    /// Highest-priority region the hop serves.
    pub region: u32,
    /// That region's priority.
    pub priority: f64,
    /// Planned duration, ns.
    pub duration_ns: i64,
}

impl Hop {
    /// Tuned centre on discovery pass `pass` (counted from 0): `center_hz` on even passes,
    /// `center_hz + dither_hz` on odd ones (T-173).
    pub fn center_on_pass(&self, pass: u64) -> f64 {
        if pass % 2 == 1 {
            self.center_hz + self.dither_hz
        } else {
            self.center_hz
        }
    }
}

/// A plan region after clipping to the source.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledRegion {
    /// As requested.
    pub requested: FreqRange,
    /// Parts inside the source's frequency ranges (empty: not coverable by this device).
    pub covered: Vec<FreqRange>,
    /// Priority.
    pub priority: f64,
    /// Revisit target, ns.
    pub revisit_ns: Option<i64>,
    /// Effective policy.
    pub policy: ScanPolicy,
}

/// Something the plan asks for that this device or budget cannot fully deliver.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanWarning {
    /// Part of the region lies outside the source's frequency ranges.
    ClippedToCapabilities {
        /// Region index.
        region: usize,
    },
    /// The whole region lies outside the source's frequency ranges; it is not scheduled.
    OutsideCapabilities {
        /// Region index.
        region: usize,
    },
    /// One discovery pass alone is longer than the region's revisit target.
    RevisitUnachievable {
        /// Region index.
        region: usize,
        /// Target, ns.
        target_ns: i64,
        /// Pass duration, ns.
        pass_ns: i64,
    },
    /// The dwell budget left by the tightest revisit target is below the minimum dwell; dwells
    /// run at the minimum and the target can be missed while POIs exist.
    DwellsCappedAtMinimum {
        /// Cap applied, ns.
        cap_ns: i64,
    },
    /// A verification group is longer than the dwell cap; each one delays the pass.
    VerificationExceedsDwellCap {
        /// Group duration, ns.
        group_ns: i64,
        /// Dwell cap, ns.
        cap_ns: i64,
    },
    /// The usable span is below 4 × `dc_dither_hz`: hops are not dithered, so a cell near a hop's
    /// LO may have no off-DC view (T-173).
    DcDitherDisabled {
        /// Configured dither, Hz.
        dither_hz: f64,
        /// Usable span, Hz.
        usable_span_hz: f64,
    },
    /// A gain-table band uses an accessory/filter port: detection trust near its edges is
    /// pending T-028 (floor-step guard hardening).
    AccessoryTrustPending {
        /// Gain-table entry.
        entry: usize,
        /// Its band.
        band: FreqRange,
    },
}

/// A ScanPlan version compiled against one source and scheduler configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct CompiledPlan {
    /// Plan id.
    pub plan_id: ScanPlanId,
    /// Plan version.
    pub plan_version: u32,
    /// Plan-level policy (`SweepOnly` disables POI dwells).
    pub policy: ScanPolicy,
    /// Regions.
    pub regions: Vec<CompiledRegion>,
    /// Discovery pass in visit order: priority descending, then frequency ascending.
    pub hops: Vec<Hop>,
    /// The plan's gain table.
    pub gain_table: Vec<GainTableEntry>,
    /// Gains outside the table.
    pub default_gains: Gains,
    /// Usable span of a discovery window, Hz.
    pub usable_span_hz: f64,
    /// DC dither of odd passes, Hz (T-173; 0: none).
    pub dc_dither_hz: f64,
    /// One discovery pass (all hops), ns.
    pub pass_ns: i64,
    /// POI dwell slots interleaved per pass.
    pub dwell_slots_per_pass: u64,
    /// Longest POI dwell: keeps `pass + slots × cap` within the tightest revisit target.
    pub dwell_cap_ns: i64,
    /// One verification group at the configured settings, ns.
    pub verification_group_ns: i64,
    /// Worst revisit of any hop when every dwell slot runs to the cap, ns (a verification group
    /// longer than the cap extends it; see [`PlanWarning::VerificationExceedsDwellCap`]).
    pub revisit_bound_ns: i64,
    /// Warnings.
    pub warnings: Vec<PlanWarning>,
}

struct Piece {
    lo: f64,
    hi: f64,
    kind: HopKind,
}

fn kind_of(policy: ScanPolicy) -> HopKind {
    match policy {
        ScanPolicy::DwellOnly => HopKind::RegionDwell,
        ScanPolicy::SweepOnly | ScanPolicy::SweepThenDwell => HopKind::Sweep,
    }
}

impl CompiledPlan {
    /// Compiles `plan` for `caps` under `cfg` (validate `cfg` first with
    /// [`SchedulerConfig::validate`]).
    ///
    /// Regions are clipped to the source's frequency ranges, merged where they overlap (per hop
    /// kind), cut at RF-path boundaries and gain-table band edges, and tiled with hops no wider
    /// than the usable span. A band narrower than half the usable span is offset-tuned so it
    /// clears the DC spike. Odd passes tune each hop `dc_dither_hz` away from its even-pass
    /// centre (T-173, see [`Hop::dither_hz`]), so every covered cell has an off-DC view within two
    /// passes.
    pub fn compile(
        plan: &ScanPlan,
        cfg: &SchedulerConfig,
        caps: &SourceCapabilities,
    ) -> Result<Self, PlanError> {
        if let Schedule::Cron { .. } = plan.schedule {
            return Err(PlanError::UnsupportedSchedule("cron"));
        }
        if plan.regions.is_empty() {
            return Err(PlanError::Empty);
        }
        if cfg.region_policy.len() > plan.regions.len() {
            return Err(PlanError::InvalidConfig(format!(
                "region_policy has {} entries for {} regions",
                cfg.region_policy.len(),
                plan.regions.len()
            )));
        }
        if plan.gain_table.len() > usize::from(u16::MAX) {
            return Err(PlanError::InvalidConfig("gain table too long".into()));
        }
        let mut warnings = Vec::new();
        for (index, e) in plan.gain_table.iter().enumerate() {
            if !(e.freq.lo_hz.is_finite()
                && e.freq.hi_hz.is_finite()
                && e.freq.hi_hz > e.freq.lo_hz)
            {
                return Err(PlanError::InvalidGain {
                    index: Some(index),
                    reason: "band must be finite with f_hi above f_lo".into(),
                });
            }
            let gains = Gains {
                lna_db: e.lna_db,
                vga_db: e.vga_db,
                amp_on: e.amp_on,
            };
            check_gains(caps, &gains).map_err(|reason| PlanError::InvalidGain {
                index: Some(index),
                reason,
            })?;
            if e.antenna_port.is_some() {
                warnings.push(PlanWarning::AccessoryTrustPending {
                    entry: index,
                    band: e.freq,
                });
            }
        }

        let mut regions = Vec::with_capacity(plan.regions.len());
        let mut pieces = Vec::new();
        for (index, r) in plan.regions.iter().enumerate() {
            let invalid = |reason: &str| PlanError::InvalidRegion {
                index,
                reason: reason.into(),
            };
            if !(r.freq.lo_hz.is_finite()
                && r.freq.hi_hz.is_finite()
                && r.freq.hi_hz > r.freq.lo_hz)
            {
                return Err(invalid("f_lo and f_hi must be finite with f_hi above f_lo"));
            }
            if !(r.priority.is_finite() && r.priority >= 0.0) {
                return Err(invalid("priority must be finite and >= 0"));
            }
            if r.revisit_ns.is_some_and(|ns| ns <= 0) {
                return Err(invalid("revisit target must be > 0"));
            }
            let policy = cfg
                .region_policy
                .get(index)
                .copied()
                .flatten()
                .unwrap_or(plan.policy);
            let kind = kind_of(policy);
            let mut covered = Vec::new();
            for range in &caps.frequency_ranges {
                let lo = r.freq.lo_hz.max(range.min_hz);
                let hi = r.freq.hi_hz.min(range.max_hz);
                if hi > lo {
                    covered.push(FreqRange::new(lo, hi));
                    pieces.push(Piece { lo, hi, kind });
                }
            }
            let covered_hz: f64 = covered.iter().map(FreqRange::width_hz).sum();
            if covered.is_empty() {
                warnings.push(PlanWarning::OutsideCapabilities { region: index });
            } else if covered_hz < r.freq.width_hz() * (1.0 - 1e-12) {
                warnings.push(PlanWarning::ClippedToCapabilities { region: index });
            }
            regions.push(CompiledRegion {
                requested: r.freq,
                covered,
                priority: r.priority,
                revisit_ns: r.revisit_ns,
                policy,
            });
        }
        if pieces.is_empty() {
            return Err(PlanError::NothingCoverable);
        }

        // Merge overlapping regions of one kind, so no frequency is visited twice per pass.
        pieces.sort_by(|a, b| a.kind.cmp(&b.kind).then(a.lo.total_cmp(&b.lo)));
        let mut lanes: Vec<Piece> = Vec::with_capacity(pieces.len());
        for p in pieces {
            match lanes.last_mut() {
                Some(l) if l.kind == p.kind && p.lo <= l.hi => l.hi = l.hi.max(p.hi),
                _ => lanes.push(p),
            }
        }
        let mut cuts: Vec<f64> = cfg.rf_path_boundaries(caps).to_vec();
        cuts.extend(
            plan.gain_table
                .iter()
                .flat_map(|e| [e.freq.lo_hz, e.freq.hi_hz]),
        );
        cuts.sort_by(f64::total_cmp);
        cuts.dedup();

        let usable_span_hz = (cfg.sweep_rate_hz * cfg.usable_fraction).min(cfg.max_span_hz);
        let dc_dither_hz = if cfg.dc_dither_hz > 0.0 && usable_span_hz < 4.0 * cfg.dc_dither_hz {
            warnings.push(PlanWarning::DcDitherDisabled {
                dither_hz: cfg.dc_dither_hz,
                usable_span_hz,
            });
            0.0
        } else {
            cfg.dc_dither_hz
        };
        let mut compiled = Self {
            plan_id: plan.id,
            plan_version: plan.version,
            policy: plan.policy,
            regions,
            hops: Vec::new(),
            gain_table: plan.gain_table.clone(),
            default_gains: cfg.default_gains,
            usable_span_hz,
            dc_dither_hz,
            pass_ns: 0,
            dwell_slots_per_pass: 0,
            dwell_cap_ns: cfg.dwell_max_ns,
            verification_group_ns: 0,
            revisit_bound_ns: 0,
            warnings,
        };
        for lane in &lanes {
            let mut lo = lane.lo;
            for &c in &cuts {
                if c > lo + EDGE_HZ && c < lane.hi - EDGE_HZ {
                    compiled.push_hops(lo, c, lane.kind, cfg, caps);
                    lo = c;
                }
            }
            compiled.push_hops(lo, lane.hi, lane.kind, cfg, caps);
        }
        compiled.hops.sort_by(|a, b| {
            b.priority
                .total_cmp(&a.priority)
                .then(a.covers.lo_hz.total_cmp(&b.covers.lo_hz))
                .then(a.kind.cmp(&b.kind))
        });
        compiled.finish_timing(cfg);
        Ok(compiled)
    }

    /// POI dwells are allowed (the plan policy is not `SweepOnly`).
    pub fn pois_allowed(&self) -> bool {
        self.policy != ScanPolicy::SweepOnly
    }

    /// Gains at `f_hz`: the first gain-table band containing it (with its index and accessory
    /// flag), else the defaults.
    pub fn gains_at(&self, f_hz: f64) -> (Gains, Option<u16>, bool) {
        for (i, e) in self.gain_table.iter().enumerate() {
            if e.freq.lo_hz <= f_hz && f_hz <= e.freq.hi_hz {
                let gains = Gains {
                    lna_db: e.lna_db,
                    vga_db: e.vga_db,
                    amp_on: e.amp_on,
                };
                return (gains, Some(i as u16), e.antenna_port.is_some());
            }
        }
        (self.default_gains, None, false)
    }

    /// Highest priority of the covered regions containing `f_hz`.
    pub fn region_priority_at(&self, f_hz: f64) -> Option<f64> {
        self.regions
            .iter()
            .filter(|r| r.covered.iter().any(|c| c.lo_hz <= f_hz && f_hz <= c.hi_hz))
            .map(|r| r.priority)
            .max_by(f64::total_cmp)
    }

    fn best_region(&self, covers: &FreqRange, kind: HopKind) -> (u32, f64) {
        let mut best: Option<(usize, f64)> = None;
        for (i, r) in self.regions.iter().enumerate() {
            let overlaps = r
                .covered
                .iter()
                .any(|c| c.lo_hz < covers.hi_hz && c.hi_hz > covers.lo_hz);
            if overlaps && kind_of(r.policy) == kind && best.is_none_or(|(_, p)| r.priority > p) {
                best = Some((i, r.priority));
            }
        }
        best.map_or((0, 0.0), |(i, p)| (i as u32, p))
    }

    fn push_hops(
        &mut self,
        lo: f64,
        hi: f64,
        kind: HopKind,
        cfg: &SchedulerConfig,
        caps: &SourceCapabilities,
    ) {
        let usable = self.usable_span_hz;
        let dither = self.dc_dither_hz;
        let width = hi - lo;
        // Slices leave `seam_guard_fraction` of the span as guard, so seams avoid the roll-off.
        let slice = usable * (1.0 - cfg.seam_guard_fraction);
        let n = ((width / slice) - 1e-9).ceil().max(1.0) as usize;
        let step = width / n as f64;
        let rate_hz = cfg.sweep_rate_hz;
        let baseband_filter_hz = pick_baseband_filter(caps, rate_hz);
        let bounds = cfg.rf_path_boundaries(caps);
        for i in 0..n {
            let c_lo = lo + step * i as f64;
            let c_hi = if i + 1 == n {
                hi
            } else {
                lo + step * (i + 1) as f64
            };
            let covers = FreqRange::new(c_lo, c_hi);
            let mid = covers.center_hz();
            let path = rf_path(bounds, mid);
            let mut center_hz = mid;
            if n == 1 && width <= usable / 2.0 {
                // Keep a narrow band off the DC spike: in the upper half, else the lower half.
                if let Some(c) = [mid - usable / 4.0, mid + usable / 4.0]
                    .into_iter()
                    .find(|&c| caps.supports_frequency(c) && rf_path(bounds, c) == path)
                {
                    center_hz = c;
                }
            }
            // T-173: the odd-pass tuning. A hop with room either side (slices of one band are
            // equally wide, so all its hops or none) moves towards the middle of the band and keeps
            // its slice inside the usable span. Otherwise every hop of the band moves up, so each
            // seam stays inside the next-lower hop's odd window, and only the band's lowest
            // `dither − room` Hz fall outside the usable span on odd passes (they are far from
            // every LO on even passes). The other direction when that leaves the source's range
            // or RF path; 0 when neither works.
            let room = (usable - covers.width_hz()) / 2.0 >= dither - EDGE_HZ;
            // "Above the middle": an offset-tuned hop's centre against its slice, else the slice
            // against its band.
            let above = if (center_hz - mid).abs() > EDGE_HZ {
                center_hz > mid
            } else {
                mid > (lo + hi) / 2.0
            };
            let first = if room && above { -dither } else { dither };
            let dither_hz = [first, -first]
                .into_iter()
                .find(|&d| {
                    let c = center_hz + d;
                    dither > 0.0 && caps.supports_frequency(c) && rf_path(bounds, c) == path
                })
                .unwrap_or(0.0);
            let (gains, gain_entry, accessory) = self.gains_at(mid);
            let (region, priority) = self.best_region(&covers, kind);
            self.hops.push(Hop {
                kind,
                center_hz,
                dither_hz,
                rate_hz,
                baseband_filter_hz,
                covers,
                gains,
                gain_entry,
                accessory,
                rf_path: path,
                region,
                priority,
                duration_ns: match kind {
                    HopKind::Sweep => cfg.sweep_step_ns,
                    HopKind::RegionDwell => cfg.region_dwell_ns,
                },
            });
        }
    }

    fn finish_timing(&mut self, cfg: &SchedulerConfig) {
        self.pass_ns = self
            .hops
            .iter()
            .fold(0i64, |acc, h| acc.saturating_add(h.duration_ns));
        self.dwell_slots_per_pass = if self.pois_allowed() {
            (self.hops.len() as u64).div_ceil(u64::from(cfg.sweeps_per_cycle))
                * u64::from(cfg.dwells_per_cycle)
        } else {
            0
        };
        let pairs = i64::from(cfg.gain_step_pairs);
        let retunes = if cfg.retune_delta_hz > 0.0 { 2 } else { 0 };
        let baseline = i64::from(pairs == 0);
        self.verification_group_ns = (2 * pairs)
            .saturating_mul(cfg.gain_step_block_ns)
            .saturating_add(
                (retunes + i64::from(cfg.rate_change) + baseline)
                    .saturating_mul(cfg.retune_dwell_ns),
            );
        let slots = i64::try_from(self.dwell_slots_per_pass).unwrap_or(i64::MAX);
        let mut tightest: Option<i64> = None;
        for (region, r) in self.regions.iter().enumerate() {
            let Some(target) = r.revisit_ns else { continue };
            if r.covered.is_empty() {
                continue;
            }
            if target < self.pass_ns {
                self.warnings.push(PlanWarning::RevisitUnachievable {
                    region,
                    target_ns: target,
                    pass_ns: self.pass_ns,
                });
            }
            tightest = Some(tightest.map_or(target, |t| t.min(target)));
        }
        self.dwell_cap_ns = cfg.dwell_max_ns;
        if let (Some(target), true) = (tightest, slots > 0) {
            let cap = (target - self.pass_ns) / slots;
            if cap < cfg.dwell_min_ns {
                self.warnings.push(PlanWarning::DwellsCappedAtMinimum {
                    cap_ns: cfg.dwell_min_ns,
                });
                self.dwell_cap_ns = cfg.dwell_min_ns;
            } else {
                self.dwell_cap_ns = cap.min(cfg.dwell_max_ns);
            }
            if self.verification_group_ns > self.dwell_cap_ns {
                self.warnings
                    .push(PlanWarning::VerificationExceedsDwellCap {
                        group_ns: self.verification_group_ns,
                        cap_ns: self.dwell_cap_ns,
                    });
            }
        }
        self.revisit_bound_ns = self
            .pass_ns
            .saturating_add(slots.saturating_mul(self.dwell_cap_ns));
    }
}

/// Checks gains against the source's `lna`/`vga` stages (range and step) and RF amp.
pub fn check_gains(caps: &SourceCapabilities, g: &Gains) -> Result<(), String> {
    if !(g.lna_db.is_finite() && g.vga_db.is_finite()) {
        return Err("gains must be finite".into());
    }
    for stage in &caps.gain_stages {
        let value = match stage.name.as_str() {
            "lna" => g.lna_db,
            "vga" => g.vga_db,
            _ => continue,
        };
        let on_grid = stage.step_db <= 0.0 || {
            let k = ((value - stage.min_db) / stage.step_db).round();
            (stage.min_db + k * stage.step_db - value).abs() < 1e-9
        };
        if !(value >= stage.min_db && value <= stage.max_db && on_grid) {
            return Err(format!(
                "{} gain {value} dB is outside {}..{} dB in {} dB steps",
                stage.name, stage.min_db, stage.max_db, stage.step_db
            ));
        }
    }
    if g.amp_on && !caps.rf_amp {
        return Err(format!("{} has no RF amplifier", caps.driver));
    }
    Ok(())
}

/// The rate to use for a window needing `needed_hz`: continuous sources round up to a multiple
/// of `quantum_hz` and clamp to the range; discrete sources take the smallest rate ≥ `needed_hz`,
/// else the largest.
pub fn pick_rate(caps: &SourceCapabilities, needed_hz: f64, quantum_hz: f64) -> f64 {
    match &caps.sample_rates {
        SampleRates::Continuous { min_hz, max_hz } => {
            (((needed_hz / quantum_hz) - 1e-9).ceil() * quantum_hz).clamp(*min_hz, *max_hz)
        }
        SampleRates::Discrete(rates) => rates
            .iter()
            .copied()
            .filter(|&r| r >= needed_hz)
            .min_by(f64::total_cmp)
            .or_else(|| rates.iter().copied().max_by(f64::total_cmp))
            .unwrap_or(needed_hz),
    }
}

/// The baseband filter for `rate_hz`: the widest ≤ 0.75 × rate (HackRF default), else the
/// narrowest; `None` when the source has no selectable filter.
pub fn pick_baseband_filter(caps: &SourceCapabilities, rate_hz: f64) -> Option<f64> {
    let target = 0.75 * rate_hz;
    match caps.baseband_filter.as_ref()? {
        BasebandFilters::Continuous { min_hz, max_hz } => Some(target.clamp(*min_hz, *max_hz)),
        BasebandFilters::Discrete(widths) => widths
            .iter()
            .copied()
            .filter(|&w| w <= target)
            .max_by(f64::total_cmp)
            .or_else(|| widths.iter().copied().min_by(f64::total_cmp)),
    }
}

/// RF path index of `f_hz`: the number of boundaries at or below it.
pub fn rf_path(boundaries_hz: &[f64], f_hz: f64) -> u8 {
    boundaries_hz.iter().take_while(|&&b| f_hz >= b).count() as u8
}
