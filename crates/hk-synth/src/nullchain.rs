//! The noise corpora calibration tables are generated from, and re-checked against (ADR-0015
//! §2.2, §13.2–§13.3; T-853 = MAUTO M-2).
//!
//! A table is the distribution of a block's `raw` metric **under its null**. For a block deep
//! in a ladder, "noise at the block's input" is not white: an S1 demodulator sees noise the S0
//! channel filter has band-limited, a clock recovery sees a demodulator's output of that. So
//! each table's null is **noise at the antenna through a canonical prefix** ending at the
//! block — a [`NullChain`] — the way docs/21 measured the M1 FSK ladder. Both the generator
//! (`examples/calibration_draws.rs` → `py/hkpy/calibrate.py`) and the Rust re-sampling check
//! (`tests/calibration_tables.rs`) draw from these chains, with different seeds.
//!
//! **The canonical prefix, and what it makes the tables valid for.**
//! - Input: complex Gaussian noise at 48 kHz, quantised as an 8-bit ADC would at a per-component
//!   σ of [`NULL_SIGMA_LSB`] LSB — inside §13.3's `nominal` bucket and inside the tighter one
//!   T-619 measured (σ ≥ 1.0 LSB, clip ≤ 10 %; ADR-0015 §16.4), with no clipping.
//! - S0: `lowpass` to a 12 kHz channel, i.e. **4× oversampled**. Band-limited noise is
//!   correlated, which widens a metric's null at a given `n`; the tables are therefore
//!   **conservative for a channel oversampled up to 4×** and generous beyond it. M-3's
//!   skeletons must keep S1's input at ≤ 4× oversampling, or regenerate.
//! - S2/S3: 4800 Bd (10 samples/symbol) through `fsk_demod`; Manchester at 9600 chips/s.
//!
//! Supports are the chain's own units (samples at S0/S1, symbols at S1 PSK and S2, bits or
//! pairs at S3); each chain enumerates two. A draw feeds enough input that the block's reported
//! `n` is **at least** the nominal support (a few percent over at S2/S3); the scorer uses the
//! largest calibrated support at or below a window's `n`.

use hk_blocks::{Evidence, PortSlice, Registry, WindowError, run_window};
use hk_recipe::Recipe;
use num_complex::Complex32;
use serde_json::{Value, json};

use crate::evidence::MetricId;

/// Per-component noise σ of the null corpus, in ADC LSB.
pub const NULL_SIGMA_LSB: f32 = 8.0;
/// Sample rate of the null corpus, Hz.
pub const NULL_RATE_HZ: f64 = 48_000.0;

/// Whether a metric is evidence when **small** (EVM, timing variance, line violations). Tables
/// store thresholds in the evidence direction (larger = more significant, ADR-0015 §13.1's
/// convention), so a scorer negates these before looking up.
pub const fn smaller_is_evidence(metric: MetricId) -> bool {
    matches!(
        metric,
        MetricId::Evm | MetricId::TimingVar | MetricId::LineViolations
    )
}

/// A metric's raw value in the evidence direction.
pub fn evidence_direction(metric: MetricId, raw: f32) -> f32 {
    if smaller_is_evidence(metric) {
        -raw
    } else {
        raw
    }
}

/// The unit of a chain's support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// Samples at the corpus rate (S0, S1 discriminators).
    Samples,
    /// Symbols at 4800 Bd (10 samples each).
    Symbols,
    /// Manchester pairs at 4800 bit/s: 9600 chips/s, 5 samples per chip, 10 per pair.
    Pairs,
}

/// One block's null corpus.
#[derive(Clone, Debug)]
pub struct NullChain {
    /// Block kind (`name`; the version comes from the registry).
    pub block: &'static str,
    /// The node id of the block under calibration (the chain's tail).
    pub node: &'static str,
    /// The two enumerated supports.
    pub supports: [u32; 2],
    /// Support unit.
    pub unit: Unit,
    nodes: Value,
}

const CHAN: &str = r#"{ "id": "chan", "block": "lowpass", "params": { "cutoff_hz": 6000, "transition_hz": 3000 } }"#;

fn node(s: &str) -> Value {
    serde_json::from_str(s).expect("static node")
}

fn chain(block: &'static str, supports: [u32; 2], unit: Unit, tail: &[&str]) -> NullChain {
    let mut nodes = vec![node(CHAN)];
    nodes.extend(tail.iter().map(|s| node(s)));
    let node_id = if tail.is_empty() {
        "chan"
    } else {
        // The last node's id, leaked once per chain (a handful of static strings).
        let v = node(tail[tail.len() - 1]);
        Box::leak(v["id"].as_str().expect("id").to_owned().into_boxed_str())
    };
    NullChain {
        block,
        node: node_id,
        supports,
        unit,
        nodes: Value::Array(nodes),
    }
}

const FSK: &str = r#"{ "id": "fsk", "block": "fsk_demod" }"#;
const CLOCK: &str = r#"{ "id": "clock", "block": "clock_recovery", "params": { "symbol_rate_bd": 4800, "pulse": "nrz", "algorithm": "gardner" } }"#;
const SLICE: &str = r#"{ "id": "slice", "block": "slicer" }"#;

/// Every chain the generator and the check run: the blocks that publish a **calibrated**
/// metric. (`subcarrier`'s pilot needs an FM-composite corpus that does not exist yet: it has no
/// table, so its `pilot_lock` scores `NoTable`, 0 bits — the designed default.)
pub fn null_chains() -> Vec<NullChain> {
    let samples = [4096, 16384];
    let symbols = [256, 1024];
    vec![
        chain("lowpass", samples, Unit::Samples, &[]),
        chain(
            "fm_demod",
            samples,
            Unit::Samples,
            &[r#"{ "id": "fm", "block": "fm_demod" }"#],
        ),
        chain(
            "am_demod",
            samples,
            Unit::Samples,
            &[r#"{ "id": "am", "block": "am_demod" }"#],
        ),
        chain("fsk_demod", samples, Unit::Samples, &[FSK]),
        chain(
            "msk_demod",
            samples,
            Unit::Samples,
            &[r#"{ "id": "msk", "block": "msk_demod", "params": { "symbol_rate_bd": 4800 } }"#],
        ),
        chain(
            "psk_demod",
            symbols,
            Unit::Symbols,
            &[
                r#"{ "id": "psk", "block": "psk_demod", "params": { "modulation": "bpsk", "symbol_rate_bd": 4800 } }"#,
            ],
        ),
        chain("clock_recovery", symbols, Unit::Symbols, &[FSK, CLOCK]),
        chain("slicer", symbols, Unit::Symbols, &[FSK, CLOCK, SLICE]),
        chain(
            "nrzi",
            symbols,
            Unit::Symbols,
            &[FSK, CLOCK, SLICE, r#"{ "id": "nrzi", "block": "nrzi" }"#],
        ),
        chain(
            "diff_decode",
            symbols,
            Unit::Symbols,
            &[
                FSK,
                CLOCK,
                SLICE,
                r#"{ "id": "diff", "block": "diff_decode" }"#,
            ],
        ),
        chain(
            "manchester",
            symbols,
            Unit::Pairs,
            &[
                FSK,
                r#"{ "id": "clock", "block": "clock_recovery", "params": { "symbol_rate_bd": 9600, "pulse": "nrz", "algorithm": "gardner" } }"#,
                r#"{ "id": "man", "block": "manchester", "params": { "align": "fixed" } }"#,
            ],
        ),
    ]
}

impl NullChain {
    /// The chain as a recipe (IQ input at [`NULL_RATE_HZ`], a `stage` output on the tail).
    pub fn recipe(&self) -> Recipe {
        serde_json::from_value(json!({
            "schema": "hackriff.recipe", "schema_version": 2,
            "id": format!("null-{}", self.block), "version": 1,
            "name": format!("calibration null for {}", self.block),
            "input": { "port": "iq", "sample_rate_hz": NULL_RATE_HZ, "bandwidth_hz": 12000 },
            "nodes": self.nodes,
            "outputs": [ { "id": "tail", "kind": "stage", "from": self.node } ],
            "output_policy": { "content_class": "metadata-only" }
        }))
        .expect("null chain recipe")
    }

    /// Input samples for a window whose reported support is at least `n`.
    pub fn samples_for(&self, n: u32) -> usize {
        let n = n as usize;
        match self.unit {
            Unit::Samples => n,
            // Clock start-up and the Gardner loop's slip: a few symbols, and 2 % for slack.
            Unit::Symbols => (n + n / 50 + 16) * 10,
            Unit::Pairs => (n + n / 50 + 16) * 10,
        }
    }

    /// One null window at support `n` (seeded): the tail block's evidence entries, each with a
    /// support of at least `n`.
    pub fn draw(
        &self,
        registry: &Registry,
        n: u32,
        seed: u64,
    ) -> Result<Vec<Evidence>, WindowError> {
        let input = quantised_noise(self.samples_for(n), seed);
        let run = run_window(
            &self.recipe(),
            registry,
            NULL_RATE_HZ,
            PortSlice::Iq(&input),
            4096,
        )?;
        let tail = run
            .nodes
            .iter()
            .find(|x| x.id == self.node)
            .expect("the tail node ran");
        Ok(tail.evidence.iter().copied().collect())
    }

    /// `name@version` of the block under calibration, from the registry.
    pub fn block_version(&self, registry: &Registry) -> String {
        use hk_recipe::Catalogue;
        let v = registry.descriptor(self.block).map_or(0, |d| d.version);
        format!("{}@{v}", self.block)
    }
}

/// Complex Gaussian noise at σ = [`NULL_SIGMA_LSB`] per component, quantised to signed 8-bit and
/// scaled to ±1 full scale, from a xorshift64* seeded by `seed`.
pub fn quantised_noise(len: usize, seed: u64) -> Vec<Complex32> {
    let mut s = seed ^ 0x9e37_79b9_7f4a_7c15;
    if s == 0 {
        s = 1;
    }
    let mut next = move || {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        s.wrapping_mul(0x2545_f491_4f6c_dd1d)
    };
    let mut uniform = move || ((next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    let sigma = f64::from(NULL_SIGMA_LSB);
    let q = |x: f64| ((x * sigma).round().clamp(-128.0, 127.0) / 128.0) as f32;
    (0..len)
        .map(|_| {
            let r = (-2.0 * uniform().ln()).sqrt();
            let th = std::f64::consts::TAU * uniform();
            Complex32::new(q(r * th.cos()), q(r * th.sin()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chain_is_a_valid_recipe_whose_tail_reports_evidence() {
        let registry = Registry::builtin();
        for c in null_chains() {
            let e = c
                .draw(&registry, c.supports[0], 1)
                .unwrap_or_else(|e| panic!("{}: {e}", c.block));
            assert!(!e.is_empty(), "{} reports nothing", c.block);
            for x in &e {
                assert!(
                    x.n >= c.supports[0],
                    "{} {:?}: n {} < {}",
                    c.block,
                    x.metric,
                    x.n,
                    c.supports[0]
                );
                assert!(
                    !x.metric.is_analytic(),
                    "{}: calibrated metrics only",
                    c.block
                );
            }
        }
    }

    #[test]
    fn the_null_corpus_is_nominally_filled_and_unclipped() {
        let x = quantised_noise(100_000, 3);
        let lsb = 1.0 / 128.0;
        let var: f64 = x.iter().map(|z| f64::from(z.re).powi(2)).sum::<f64>() / x.len() as f64;
        let sigma_lsb = var.sqrt() / lsb;
        assert!(
            (sigma_lsb - f64::from(NULL_SIGMA_LSB)).abs() < 0.2,
            "{sigma_lsb}"
        );
        let clipped = x.iter().filter(|z| z.re.abs() >= 127.0 / 128.0).count();
        assert_eq!(clipped, 0);
    }
}
