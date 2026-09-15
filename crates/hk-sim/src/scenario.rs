//! Seeded emitter populations (the hidden truth) and the plan regions they are surveyed under.

use hk_model::{FreqRange, PlanRegion, ScanPlan, ScanPolicy, Schedule, Timestamp};
use serde::Serialize;

use crate::rng::Rng;

/// Nanoseconds per second.
pub const S: i64 = 1_000_000_000;
/// Nanoseconds per millisecond.
pub const MS: i64 = 1_000_000;
/// Hz per MHz.
pub const MHZ: f64 = 1e6;
/// Simulation epoch: 2026-09-13T00:00:00Z. Scheduler timestamps are `T0_NS + t`.
pub const T0_NS: i64 = 1_789_257_600_000_000_000;
/// Lowest and highest emitter frequency (HackRF One range).
const RANGE_HZ: (f64, f64) = (1.0 * MHZ, 6000.0 * MHZ);

/// Emitter class of the truth population.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmitterClass {
    /// Continuous carrier (broadcast, control channel).
    Carrier,
    /// Poisson bursts, τ 5–100 ms (meters, remotes, telemetry).
    Burst,
    /// Periodic beacon (30 s, 60 s, minutes: weather sensors, TPMS).
    Beacon,
    /// Frequency hopper: each transmission on another channel.
    Hopper,
    /// Intermodulation ghost, tagged suspect (C05).
    ImdGhost,
}

impl EmitterClass {
    /// All classes in report order.
    pub const ALL: [Self; 5] = [
        Self::Carrier,
        Self::Burst,
        Self::Beacon,
        Self::Hopper,
        Self::ImdGhost,
    ];

    /// Stable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Carrier => "carrier",
            Self::Burst => "burst",
            Self::Beacon => "beacon",
            Self::Hopper => "hopper",
            Self::ImdGhost => "imd-ghost",
        }
    }
}

/// When an emitter transmits.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Activity {
    /// On from `appears_ns` to the end of the run.
    Continuous,
    /// Bursts of `tau_ns`, separated by exponential gaps of mean `mean_gap_ns` (thinned by the
    /// time-of-day factor).
    Poisson {
        /// Burst duration, ns.
        tau_ns: i64,
        /// Mean gap between bursts at the diurnal peak, ns.
        mean_gap_ns: i64,
    },
    /// A burst of `tau_ns` every `period_ns`, at `phase_ns` + k·period ± `jitter_ns`.
    Periodic {
        /// Period, ns.
        period_ns: i64,
        /// Burst duration, ns.
        tau_ns: i64,
        /// Phase, ns.
        phase_ns: i64,
        /// Uniform jitter half-width, ns.
        jitter_ns: i64,
    },
    /// Poisson-timed transmissions of `tau_ns`, each on a random channel.
    Hopping {
        /// Centre of channel 0, Hz.
        channel0_hz: f64,
        /// Channel spacing, Hz.
        spacing_hz: f64,
        /// Channel count.
        channels: u32,
        /// Transmission (hop) duration, ns.
        tau_ns: i64,
        /// Mean gap between hops, ns.
        mean_gap_ns: i64,
    },
}

/// One truth emitter. Hidden from policies: they only see [`crate::Detection`]s.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EmitterSpec {
    /// Index in [`Scenario::emitters`]; also the tracker key a detection carries.
    pub id: u64,
    /// Class.
    pub class: EmitterClass,
    /// Centre, Hz (a hopper: the centre of its channel span).
    pub center_hz: f64,
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Mean in-band SNR, dB.
    pub snr_db: f64,
    /// Per-transmission SNR standard deviation, dB.
    pub fade_db: f64,
    /// Tagged suspect (IMD ghost, spur) by the C05 trust tests.
    pub suspect: bool,
    /// First instant it can transmit, ns from the run start.
    pub appears_ns: i64,
    /// Injected mid-run (novelty target for time-to-first-detection).
    pub injected: bool,
    /// Time-of-day modulation depth in `[0, 1]` (Poisson and hopping activity only).
    pub diurnal_depth: f64,
    /// Activity.
    pub activity: Activity,
}

impl EmitterSpec {
    /// Frequency span its transmissions can occupy (centres), Hz.
    pub fn span_hz(&self) -> (f64, f64) {
        match self.activity {
            Activity::Hopping {
                channel0_hz,
                spacing_hz,
                channels,
                ..
            } => (
                channel0_hz,
                channel0_hz + spacing_hz * f64::from(channels.saturating_sub(1)),
            ),
            _ => (self.center_hz, self.center_hz),
        }
    }

    /// Burst duration, if bounded.
    pub fn tau_ns(&self) -> Option<i64> {
        match self.activity {
            Activity::Continuous => None,
            Activity::Poisson { tau_ns, .. }
            | Activity::Periodic { tau_ns, .. }
            | Activity::Hopping { tau_ns, .. } => Some(tau_ns),
        }
    }
}

/// A band emitters of one class are drawn from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Band {
    /// Low edge, Hz.
    pub lo_hz: f64,
    /// High edge, Hz.
    pub hi_hz: f64,
    /// Emitter bandwidth in this band, Hz.
    pub bandwidth_hz: f64,
}

/// `Band` from MHz edges and a kHz bandwidth.
pub fn band(lo_mhz: f64, hi_mhz: f64, bw_khz: f64) -> Band {
    Band {
        lo_hz: lo_mhz * MHZ,
        hi_hz: hi_mhz * MHZ,
        bandwidth_hz: bw_khz * 1e3,
    }
}

/// A frequency hopper to place.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HopperConfig {
    /// Channel 0 centre, Hz.
    pub channel0_hz: f64,
    /// Channel spacing, Hz.
    pub spacing_hz: f64,
    /// Channels.
    pub channels: u32,
    /// Bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Hop duration, ms.
    pub tau_ms: f64,
    /// Mean gap between hops, ms.
    pub mean_gap_ms: f64,
    /// SNR, dB.
    pub snr_db: f64,
}

/// Recipe for a seeded population. [`ScenarioConfig::default`] is a mixed urban-ish population
/// over the whole HackRF range.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScenarioConfig {
    /// Name.
    pub name: String,
    /// Plan regions `(lo, hi)`, Hz, priority 1, no revisit target.
    pub regions_hz: Vec<(f64, f64)>,
    /// Continuous carriers.
    pub carriers: usize,
    /// Carrier bands.
    pub carrier_bands: Vec<Band>,
    /// Carrier SNR range, dB.
    pub carrier_snr_db: (f64, f64),
    /// Poisson burst emitters.
    pub bursts: usize,
    /// Burst bands.
    pub burst_bands: Vec<Band>,
    /// Burst duration range, ms (uniform per emitter).
    pub burst_tau_ms: (f64, f64),
    /// Mean gap range, s (log-uniform per emitter).
    pub burst_mean_gap_s: (f64, f64),
    /// Burst and beacon SNR range, dB.
    pub burst_snr_db: (f64, f64),
    /// Periodic beacons.
    pub beacons: usize,
    /// Beacon bands.
    pub beacon_bands: Vec<Band>,
    /// Beacon periods to pick from, s.
    pub beacon_periods_s: Vec<f64>,
    /// Beacon burst duration range, ms.
    pub beacon_tau_ms: (f64, f64),
    /// Hoppers.
    pub hoppers: Vec<HopperConfig>,
    /// IMD ghosts (at 2·f1 − f2 of two carriers), tagged suspect.
    pub imd_ghosts: usize,
    /// Classes of emitters injected mid-run (one each).
    pub injected: Vec<EmitterClass>,
    /// When injected emitters appear, as a fraction of the run.
    pub inject_at_fraction: f64,
    /// Per-transmission SNR standard deviation, dB.
    pub fade_db: f64,
    /// Time-of-day modulation depth for bursts and hoppers (0 disables).
    pub diurnal_depth: f64,
    /// Local time of day at the run start, s.
    pub start_time_of_day_s: f64,
    /// Hour of peak activity.
    pub diurnal_peak_hour: f64,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        Self {
            name: "default-mixed".into(),
            regions_hz: vec![
                (1.0 * MHZ, 300.0 * MHZ),
                (300.0 * MHZ, 1000.0 * MHZ),
                (1000.0 * MHZ, 3000.0 * MHZ),
                (3000.0 * MHZ, 6000.0 * MHZ),
            ],
            carriers: 40,
            carrier_bands: vec![
                band(88.0, 108.0, 200.0),
                band(150.0, 174.0, 12.5),
                band(450.0, 470.0, 12.5),
                band(925.0, 960.0, 200.0),
                band(1805.0, 1880.0, 200.0),
                band(2110.0, 2170.0, 5000.0),
            ],
            carrier_snr_db: (10.0, 40.0),
            bursts: 60,
            burst_bands: vec![
                band(150.0, 174.0, 12.5),
                band(314.0, 316.0, 50.0),
                band(433.05, 434.79, 50.0),
                band(450.0, 470.0, 12.5),
                band(868.0, 870.0, 100.0),
                band(902.0, 928.0, 200.0),
                band(2400.0, 2483.5, 1000.0),
            ],
            burst_tau_ms: (5.0, 100.0),
            burst_mean_gap_s: (2.0, 120.0),
            burst_snr_db: (8.0, 30.0),
            beacons: 20,
            beacon_bands: vec![
                band(314.0, 316.0, 50.0),
                band(433.05, 434.79, 50.0),
                band(868.0, 870.0, 100.0),
                band(902.0, 928.0, 200.0),
            ],
            beacon_periods_s: vec![30.0, 60.0, 120.0, 300.0],
            beacon_tau_ms: (10.0, 200.0),
            hoppers: vec![
                HopperConfig {
                    channel0_hz: 902.5 * MHZ,
                    spacing_hz: 0.5 * MHZ,
                    channels: 50,
                    bandwidth_hz: 250e3,
                    tau_ms: 400.0,
                    mean_gap_ms: 100.0,
                    snr_db: 20.0,
                },
                HopperConfig {
                    channel0_hz: 2402.0 * MHZ,
                    spacing_hz: 2.0 * MHZ,
                    channels: 40,
                    bandwidth_hz: 1e6,
                    tau_ms: 5.0,
                    mean_gap_ms: 200.0,
                    snr_db: 15.0,
                },
            ],
            imd_ghosts: 10,
            injected: vec![EmitterClass::Burst, EmitterClass::Beacon],
            inject_at_fraction: 0.5,
            fade_db: 2.0,
            diurnal_depth: 0.5,
            start_time_of_day_s: 0.0,
            diurnal_peak_hour: 14.0,
        }
    }
}

/// A population plus the plan regions it is surveyed under.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Scenario {
    /// Name.
    pub name: String,
    /// Seed it was generated from.
    pub seed: u64,
    /// Run length, ns.
    pub duration_ns: i64,
    /// Plan regions.
    pub regions: Vec<FreqRange>,
    /// Truth emitters; `emitters[i].id == i`.
    pub emitters: Vec<EmitterSpec>,
    /// Local time of day at the run start, s.
    pub start_time_of_day_s: f64,
    /// Hour of peak activity.
    pub diurnal_peak_hour: f64,
}

impl Scenario {
    /// Generates a population from `cfg` and `seed` for a run of `duration_ns`.
    pub fn generate(cfg: &ScenarioConfig, seed: u64, duration_ns: i64) -> Self {
        let mut rng = Rng::derive(seed, u64::MAX);
        let mut emitters: Vec<EmitterSpec> = Vec::new();
        let push = |emitters: &mut Vec<EmitterSpec>, mut e: EmitterSpec| {
            e.id = emitters.len() as u64;
            emitters.push(e);
        };
        for _ in 0..cfg.carriers {
            let e = carrier(cfg, &mut rng);
            push(&mut emitters, e);
        }
        for _ in 0..cfg.bursts {
            let e = burst(cfg, &mut rng);
            push(&mut emitters, e);
        }
        for _ in 0..cfg.beacons {
            let e = beacon(cfg, &mut rng);
            push(&mut emitters, e);
        }
        for h in &cfg.hoppers {
            let (lo, hi) = (
                h.channel0_hz,
                h.channel0_hz + h.spacing_hz * f64::from(h.channels.saturating_sub(1)),
            );
            let e = EmitterSpec {
                id: 0,
                class: EmitterClass::Hopper,
                center_hz: (lo + hi) / 2.0,
                bandwidth_hz: h.bandwidth_hz,
                snr_db: h.snr_db,
                fade_db: cfg.fade_db,
                suspect: false,
                appears_ns: 0,
                injected: false,
                diurnal_depth: cfg.diurnal_depth,
                activity: Activity::Hopping {
                    channel0_hz: h.channel0_hz,
                    spacing_hz: h.spacing_hz,
                    channels: h.channels,
                    tau_ns: ms(h.tau_ms),
                    mean_gap_ns: ms(h.mean_gap_ms),
                },
            };
            push(&mut emitters, e);
        }
        let carriers: Vec<(f64, f64)> = emitters
            .iter()
            .filter(|e| e.class == EmitterClass::Carrier)
            .map(|e| (e.center_hz, e.bandwidth_hz))
            .collect();
        for _ in 0..cfg.imd_ghosts {
            let e = ghost(cfg, &carriers, &mut rng);
            push(&mut emitters, e);
        }
        let inject_at = (duration_ns as f64 * cfg.inject_at_fraction.clamp(0.0, 1.0)) as i64;
        for &class in &cfg.injected {
            let mut e = match class {
                EmitterClass::Carrier => carrier(cfg, &mut rng),
                EmitterClass::Burst => burst(cfg, &mut rng),
                EmitterClass::Beacon => beacon(cfg, &mut rng),
                EmitterClass::ImdGhost => ghost(cfg, &carriers, &mut rng),
                EmitterClass::Hopper => continue,
            };
            e.appears_ns = inject_at;
            e.injected = true;
            push(&mut emitters, e);
        }
        Self {
            name: cfg.name.clone(),
            seed,
            duration_ns,
            regions: cfg
                .regions_hz
                .iter()
                .map(|&(lo, hi)| FreqRange::new(lo, hi))
                .collect(),
            emitters,
            start_time_of_day_s: cfg.start_time_of_day_s,
            diurnal_peak_hour: cfg.diurnal_peak_hour,
        }
    }

    /// The ScanPlan (version 1) over the scenario's regions under `policy`, with `extra`.
    pub fn plan(&self, policy: ScanPolicy, extra: serde_json::Value) -> ScanPlan {
        ScanPlan {
            id: "01926f3a-0000-7000-8000-000000000114"
                .parse()
                .expect("valid plan id"),
            version: 1,
            name: format!("sim:{}", self.name),
            created_at: Timestamp::from_unix_nanos(T0_NS),
            regions: self
                .regions
                .iter()
                .map(|&freq| PlanRegion {
                    freq,
                    priority: 1.0,
                    revisit_ns: None,
                })
                .collect(),
            policy,
            gain_table: Vec::new(),
            schedule: Schedule::Continuous,
            extra,
        }
    }

    /// Index of the first region containing `f_hz`.
    pub fn region_of(&self, f_hz: f64) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| f_hz >= r.lo_hz && f_hz < r.hi_hz)
    }

    /// Time-of-day activity factor at `t_ns` in `[1 − depth, 1 + depth]`.
    pub fn diurnal_factor(&self, depth: f64, t_ns: i64) -> f64 {
        if depth <= 0.0 {
            return 1.0;
        }
        let tod_s = (self.start_time_of_day_s + t_ns as f64 / S as f64).rem_euclid(86_400.0);
        let phase = (tod_s - self.diurnal_peak_hour * 3600.0) / 86_400.0;
        1.0 + depth * (std::f64::consts::TAU * phase).cos()
    }
}

fn ms(v: f64) -> i64 {
    (v * MS as f64).round().max(1.0) as i64
}

fn in_band(rng: &mut Rng, bands: &[Band]) -> (f64, f64) {
    let b = rng.pick(bands);
    let half = b.bandwidth_hz / 2.0;
    let lo = b.lo_hz + half;
    let hi = (b.hi_hz - half).max(lo);
    (rng.uniform(lo, hi), b.bandwidth_hz)
}

fn base(cfg: &ScenarioConfig, class: EmitterClass, f: (f64, f64), snr_db: f64) -> EmitterSpec {
    EmitterSpec {
        id: 0,
        class,
        center_hz: f.0,
        bandwidth_hz: f.1,
        snr_db,
        fade_db: cfg.fade_db,
        suspect: false,
        appears_ns: 0,
        injected: false,
        diurnal_depth: 0.0,
        activity: Activity::Continuous,
    }
}

fn carrier(cfg: &ScenarioConfig, rng: &mut Rng) -> EmitterSpec {
    let f = in_band(rng, &cfg.carrier_bands);
    let snr = rng.uniform(cfg.carrier_snr_db.0, cfg.carrier_snr_db.1);
    base(cfg, EmitterClass::Carrier, f, snr)
}

fn burst(cfg: &ScenarioConfig, rng: &mut Rng) -> EmitterSpec {
    let f = in_band(rng, &cfg.burst_bands);
    let snr = rng.uniform(cfg.burst_snr_db.0, cfg.burst_snr_db.1);
    let mut e = base(cfg, EmitterClass::Burst, f, snr);
    e.diurnal_depth = cfg.diurnal_depth;
    e.activity = Activity::Poisson {
        tau_ns: ms(rng.uniform(cfg.burst_tau_ms.0, cfg.burst_tau_ms.1)),
        mean_gap_ns: (rng.log_uniform(cfg.burst_mean_gap_s.0, cfg.burst_mean_gap_s.1) * S as f64)
            as i64,
    };
    e
}

fn beacon(cfg: &ScenarioConfig, rng: &mut Rng) -> EmitterSpec {
    let f = in_band(rng, &cfg.beacon_bands);
    let snr = rng.uniform(cfg.burst_snr_db.0, cfg.burst_snr_db.1);
    let mut e = base(cfg, EmitterClass::Beacon, f, snr);
    let period_ns = (rng.pick(&cfg.beacon_periods_s) * S as f64) as i64;
    e.activity = Activity::Periodic {
        period_ns,
        tau_ns: ms(rng.uniform(cfg.beacon_tau_ms.0, cfg.beacon_tau_ms.1)),
        phase_ns: rng.below(period_ns as u64) as i64,
        jitter_ns: period_ns / 200,
    };
    e
}

fn ghost(cfg: &ScenarioConfig, carriers: &[(f64, f64)], rng: &mut Rng) -> EmitterSpec {
    let mut f = None;
    if carriers.len() >= 2 {
        for _ in 0..16 {
            let a = rng.pick(carriers);
            let b = rng.pick(carriers);
            let g = 2.0 * a.0 - b.0;
            if a.0 != b.0 && g > RANGE_HZ.0 && g < RANGE_HZ.1 {
                f = Some((g, a.1));
                break;
            }
        }
    }
    let f = f.unwrap_or_else(|| in_band(rng, &cfg.carrier_bands));
    let mut e = base(cfg, EmitterClass::ImdGhost, f, rng.uniform(8.0, 15.0));
    e.suspect = true;
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_population_is_seeded_and_indexed() {
        let a = Scenario::generate(&ScenarioConfig::default(), 3, 3600 * S);
        let b = Scenario::generate(&ScenarioConfig::default(), 3, 3600 * S);
        let c = Scenario::generate(&ScenarioConfig::default(), 4, 3600 * S);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.emitters.iter().enumerate().all(|(i, e)| e.id == i as u64));
        assert!(
            a.emitters
                .iter()
                .all(|e| e.center_hz > RANGE_HZ.0 && e.center_hz < RANGE_HZ.1)
        );
        assert_eq!(a.emitters.iter().filter(|e| e.injected).count(), 2);
        assert!(
            a.emitters
                .iter()
                .all(|e| e.suspect == (e.class == EmitterClass::ImdGhost))
        );
        let d = a.diurnal_factor(0.5, 14 * 3600 * S);
        assert!((d - 1.5).abs() < 1e-9);
    }
}
