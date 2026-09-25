//! **The false-confirm acceptance suite** (T-576; ADR-0022 §10, docs/22 §3) — `SIGNAL-001`,
//! `SIGNAL-062`, `RESEARCH-009`.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto) & test(/false_confirm/)'
//! ```
//!
//! The user's budget is **one wrong Confirmed emitter per unattended device-week**, and
//! `ConfirmPolicy.synthesized` (T-575) is the gate that is supposed to deliver it. This suite is
//! the only evidence that it does. It runs T-568's negative populations — it **extends** them and
//! rebuilds nothing — through the mock SDR, blind, and makes ADR-0022 §10.1's three assertions,
//! which exist because **one of them has no power and one of them is passed by doing nothing**:
//!
//! | | What it asserts | Why it is not enough on its own |
//! |---|---|---|
//! | **A1** | over N1 ∪ N2 ∪ N4 at each of `standard` and `deep`: `confirms == 0` and `emitters_created == 0` | necessary and **75× too weak** to demonstrate the budget (§10.2, and [`honest_bound`]) |
//! | **A2** | the **maximum `analytic_holdout_bits` reached by any rank-1 hold-out result on input with nothing to find** ≤ [`a2_threshold_bits`]`(n)`, with p50/p99 per population per profile and the upper-tail slope | this is the assertion with power: it measures ADR-0022 §3.2's 9.7-bit model margin *directly*, instead of waiting for a crossing a 24-bit gate makes vanishingly rare. **If the observed max is 21 bits the true margin is 3 bits, not 9.7, and the budget is missed by ~100× while A1 still reads zero.** |
//! | **A3** | the recall control: ADR-0015 §7's positive rows still confirm under the new gate | without it the suite is passed by **never confirming** |
//!
//! # A2's threshold is a function of `n`, not a constant (docs/22 §3)
//!
//! A sample of `n` negative decisions resolves a tail location no finer than `log₂(n)` bits, so
//! ADR-0022's `18.0` is the constant **only at docs/22's frozen n = 1600** ([`A2_FROZEN_N`]), where
//! it fires once the realised optimism exceeds [`A2_FIRES_ABOVE_BITS`] = 7.4 bits — conservative
//! by 2.3 bits against the assumed 9.7. At n = 200 the same constant would mean "optimism ≤ 10.3
//! bits", i.e. **no power at all while still reading green**. So the suite asserts against
//! `log₂(n_actual) + 7.4` and reports both, and a suite that silently shrinks its negative
//! population gets a *harder* assertion rather than a free pass
//! ([`a2_threshold_is_a_function_of_n_so_a_shrunken_population_cannot_pass_silently`]).
//!
//! # What the product can be asked today — stated, not papered over
//!
//! **There is no production search backend.** `hk_pipeline::synth::jobs::server_backend()` is
//! `None`: nothing implements the engine's `Evaluator` over acquired IQ, so every region-analyze
//! job **acquires its window for real** and ends `failed / no_evaluator` (ADR-0021 §7A.4's
//! *not-searched*, which is not *unknown*). Consequences, each reported rather than hidden:
//!
//! - **A1 binds today** and is not vacuous in the plumbing sense: the jobs are really started
//!   through `POST /api/analyze`, really acquire IQ, and the suite asserts that **nothing was
//!   confirmed and no emitter was created by the analyze/attach path** — measured from the served
//!   inventory (a row whose `lifecycle.actor` is `hk-pipeline/confirm-synth@2`, and any row that
//!   appeared after the jobs ran). It is, however, A1 over **zero searched decisions**, which is a
//!   weaker fact than A1 over 800, and the report says so in those words.
//! - **A2 is armed, not measured.** With no rank-1 hold-out result there is no tail to locate;
//!   `n = 0` and [`a2_threshold_bits`] is `None`. The moment a backend lands, every observation
//!   flows into the same pure [`assess`] with no edit here, and the failure message below fires.
//! - **A3 is a gate-level recall control** over the shipped `SynthesizedConfirm`, because the
//!   engine-side half ("≥ 14/16 squitters `solved`") cannot be measured while nothing searches —
//!   and a recall control that is itself unmeasured is exactly the "passed by doing nothing" hole
//!   A3 exists to close. What is engine-side and where it will be measured is
//!   [`ENGINE_SIDE_ROWS`].
//!
//! # What this suite honestly bounds (ADR-0022 §10.2, and never more than this)
//!
//! Zero confirms over `n` negative decisions bounds the true per-decision false-confirm rate to
//! **≤ 3/n at 95 % confidence** (the rule of three). At ADR-0022's n = 800 per profile that is
//! **3.7 × 10⁻³**, against a budget of **5.0 × 10⁻⁵**: **A1 is 75× too weak to demonstrate the
//! budget and must never be quoted as doing so.** It can only fail to contradict it. Every run
//! prints that paragraph with the `n` that actually ran ([`honest_bound`]), and the confidence
//! comes from A2, which locates a tail rather than estimating a rare rate.
//!
//! # The n ≈ 60 000 question — this ticket's stated position
//!
//! Demonstrating 5 × 10⁻⁵ from a zero-failure run at 95 % needs **n ≈ 60 000** negative decisions
//! (3/n ≤ 5 × 10⁻⁵). ADR-0022 §10.2 hands the build-or-not decision to T-576. The position, with
//! its reasons, is [`N60K_POSITION`] and it is printed by every run:
//!
//! 1. **Build it, as a milestone/nightly batch, never in CI.** 60 000 region-analyze jobs at the
//!    ~1–3 s a negative job costs is 17–50 h serial, 3–8 h at the six-way concurrency the `e2e`
//!    group already runs at. That is a `just timing`-class run on a quiet box (docs/10 §3.6), and
//!    it is two orders of magnitude outside a 20-minute merge gate.
//! 2. **Not yet, and not because of cost:** while `server_backend()` is `None` a negative *job* is
//!    not a negative *decision*. 60 000 jobs ending `no_evaluator` would bound nothing at all
//!    while producing a very convincing-looking number — the docs/17 §1 four-decimal error with a
//!    bigger n. The run becomes meaningful the day a backend searches.
//! 3. **It buys the rate claim (C-L/A1), not the margin claim (C-M/A2)**, which docs/22 §3 already
//!    shows is better served by n = 1600 and the tail slope. The two must not be conflated when
//!    someone decides to spend the hours.
//!
//! # Protocol (ADR-0021 §8.4, non-negotiable)
//!
//! Fixtures replay **through the mock SDR** behind the ordinary device interface; jobs start via
//! `POST /api/analyze`; targets are whatever **blind detection** produced (never a truth
//! frequency); truth is loaded only by the assert harness (here: only to name the fixture in a
//! failure message); jobs are bounded by the profile's `max_evaluations`, never by wall — nothing
//! here sets `max_wall_s`.
//!
//! **Both target kinds run.** Every blind-detected emitter is analysed, *and* so is an **ad-hoc
//! band** per retained IQ-ring segment, spanning the window the device itself reports it sampled
//! (`GET /api/iqbuffer`; [`band_targets`]). The second kind is not optional here: thermal noise
//! must produce **no** emitter (T-568's N1 rule), so an emitter-only suite would make zero confirm
//! decisions over the population A1 needs most and pass by having asked nothing.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use hk_pipeline::inventory::{CONFIRM_SYNTH_RULE, SynthesizedConfirm};
use hk_store::iqbuffer::IqBufferConfig;
use serde_json::{Value, json};

use crate::blind::{BlindSource, blind_config, start};
use crate::common::*;
use crate::mauto_eval::{serve_with_analyze, wait_job};
use crate::mauto_negatives::{Population, SUBPOPS, Source, SubPop};
use crate::signal_087::api_post;

const USE_CASES: &str = "SIGNAL-001 / SIGNAL-062 / RESEARCH-009 (T-576, ADR-0022 §10)";

// ------------------------------------------------------------------------------------------
// ADR-0022's numbers. Every one of them is derived in the ADR, and none is re-derived here.
// ------------------------------------------------------------------------------------------

/// ADR-0022 §1.1: the budgeted per-decision false-confirm rate (one wrong Confirmed emitter per
/// unattended device-week at [`DECISIONS_PER_WEEK`]).
pub const BUDGET_PER_DECISION: f64 = 5.0e-5;
/// ADR-0022 §1.2: the budget's denominator, confirm decisions per device-week.
pub const DECISIONS_PER_WEEK: f64 = 20_000.0;
/// ADR-0022 §3.1: `B = log2(20 000)`, the budget in bits.
pub const BUDGET_BITS: f64 = 14.29;
/// ADR-0022 §3.2: `M`, the **stated, unmeasured** model-error margin inside the threshold.
pub const ASSUMED_MARGIN_BITS: f64 = 9.7;
/// ADR-0022 §4.1: the gate, `B + M`.
pub const THRESHOLD_BITS: f64 = 24.0;

/// ADR-0022 §10.1's A2 constant — correct **only** at [`A2_FROZEN_N`]; see [`a2_threshold_bits`].
pub const A2_CONSTANT_BITS: f64 = 18.0;
/// docs/22 §3: the `n` the constant is frozen alongside (pooled, the CI tier of the corpus).
pub const A2_FROZEN_N: usize = 1600;
/// docs/22 §3: the optimism, in bits, above which A2 fires at any `n` — 7.4, conservative by 2.3
/// bits against [`ASSUMED_MARGIN_BITS`].
pub const A2_FIRES_ABOVE_BITS: f64 = 7.4;

/// The profiles A1 is asserted at (ADR-0022 §10.1; `quick` never confirms, so it is not a
/// population where a confirm could occur).
pub const PROFILES: &[&str] = &["standard", "deep"];

/// The populations A1 runs over (ADR-0022 §10.1: N1 thermal, N2 energy without symbols, N4 the
/// real empty capture). N3 and N5 stay T-568's, scored there and by its own rules.
pub const A1_POPULATIONS: &[Population] = &[Population::N1, Population::N2, Population::N4];

/// Seeds drawn per synthetic sub-population at CI tier. Deliberately small: each seed is a whole
/// pipeline run **plus** one region-analyze job per emitter per profile. It is not docs/22's n, it
/// is not a rate measurement, and [`a2_threshold_bits`] tightens with it rather than loosening.
pub const CI_SEEDS: std::ops::RangeInclusive<u64> = 1..=2;

/// A2's assertion threshold at the `n` that actually ran: `log₂(n) + 7.4` (docs/22 §3). `None`
/// when `n == 0` — with no decision there is no tail, and a threshold would be arithmetic over
/// nothing.
pub fn a2_threshold_bits(n: usize) -> Option<f64> {
    (n > 0).then(|| (n as f64).log2() + A2_FIRES_ABOVE_BITS)
}

/// ADR-0015 §7's positive rows whose **engine side** cannot be measured while
/// `server_backend()` is `None`, with what each must do and where it is measured when it can be.
/// Listed so the review has them in one place and so nobody reads A3 as covering them.
pub const ENGINE_SIDE_ROWS: &[(&str, &str)] = &[
    (
        "ADS-B squitter scene, burst path",
        "≥ 14/16 single squitters `solved` and the burst set confirms — `mauto_eval`'s armed \
         rows (T-863) plus `tutorial_adsb`'s decode path",
    ),
    (
        "a CRC-8 emitter in 902–928 MHz, template-fixed",
        "reaches `solved` on hold-out over real IQ — the check axis of the corpus's 2fsk rows \
         (`fsk_burst_train`, T-622), which needs a production search backend",
    ),
    (
        "RDS on fm_100p8M, POCSAG, ACARS",
        "confirm as ADR-0015 §7 already requires — `tutorial_rds`, `tutorial_pocsag`, \
         `tutorial_acars` for the decode; the synthesized route needs a backend",
    ),
];

/// This ticket's stated position on ADR-0022 §10.2's n ≈ 60 000 run (module docs give the
/// reasons). Printed by every run so the decision travels with the number it is about.
pub const N60K_POSITION: &str = "\
    the n ≈ 60 000 run (ADR-0022 §10.2): WORTH BUILDING, as a milestone/nightly batch on a quiet \
    box (`just timing` class, 3–8 h at six-way concurrency), and NOT YET — while \
    server_backend() is None a negative job is not a negative decision, so 60 000 jobs ending \
    no_evaluator would bound nothing while looking authoritative. It buys the RATE claim (A1), \
    not the margin claim (A2), which docs/22 §3 shows is better served by n = 1600 plus the tail \
    slope. Never in CI: it is two orders of magnitude outside the merge gate.";

// ------------------------------------------------------------------------------------------
// One negative decision, and one run.
// ------------------------------------------------------------------------------------------

/// The rank-1 hold-out result of one decision, with the arithmetic ADR-0022 §10.2's failure
/// message has to print.
#[derive(Clone, Debug, PartialEq)]
pub struct Winner {
    /// `analytic_holdout_bits` — the confirm key (ADR-0022 §2).
    pub bits: f64,
    /// The check model, e.g. `CRC-16`.
    pub check: String,
    /// Check width, bits.
    pub width: Option<u64>,
    /// ADR-0022 §4.2's `differences` (never `distinct_valid`).
    pub differences: Option<u64>,
    /// `L_check`, the charge for the search that found the check.
    pub l_check: Option<f64>,
    /// Whether the check was searched rather than template-fixed.
    pub check_searched: Option<bool>,
    /// ADR-0021 §8.2's null-control margin, when the control ran.
    pub null_margin_bits: Option<f64>,
    /// Whether the null control capped the result.
    pub null_capped: Option<bool>,
}

/// One **confirm decision** on input with nothing to find: one region-analyze job over one target,
/// at one profile.
#[derive(Clone, Debug)]
pub struct Decision {
    /// The sub-population's manifest level (`n1-thermal`, …).
    pub level: &'static str,
    /// Its population.
    pub pop: Population,
    /// The seed, or `None` for the real window.
    pub seed: Option<u64>,
    /// The profile the job asked for.
    pub profile: &'static str,
    /// The job target, for a failure message (an emitter id).
    pub target: String,
    /// The served job state: `done` · `failed` · `cancelled` · `refused:<status>/<code>`.
    pub state: String,
    /// `error.code` when it failed — `no_evaluator` on this build.
    pub error: Option<String>,
    /// `confirm.outcome` (`confirmed` · `already` · `insufficient` · `not-attached`).
    pub confirm_outcome: Option<String>,
    /// `confirm.decision_rate.budget_claim`, when the rule was evaluated.
    pub budget_claim: Option<String>,
    /// The rank-1 hold-out result, when the job produced one.
    pub holdout: Option<Winner>,
}

impl Decision {
    /// A **searched** decision: a job that ran to `done`, so the gate could have been reached.
    /// A failed or cancelled job ruled nothing out (ADR-0021 §7A.4) and is not a decision A2's
    /// `n` may count.
    pub fn searched(&self) -> bool {
        self.state == "done"
    }

    /// Whether this decision confirmed an emitter.
    pub fn confirmed(&self) -> bool {
        self.confirm_outcome.as_deref() == Some("confirmed")
    }
}

/// One negative scene, run through the mock SDR: its decisions, and what the analyze/attach path
/// did to the inventory.
#[derive(Clone, Debug, Default)]
pub struct Run {
    /// The sub-population's manifest level.
    pub level: &'static str,
    /// The seed, or `None`.
    pub seed: Option<u64>,
    /// The recording the mock device served, for the replay command in a failure.
    pub fixture: String,
    /// Emitters blind detection produced before any job ran (T-568's business, reported here).
    pub emitters_before: usize,
    /// Inventory rows that appeared **after** the jobs ran: emitters the analyze/attach path
    /// created. A1 allows none.
    pub emitters_created: Vec<String>,
    /// Rows whose `lifecycle.actor` is [`CONFIRM_SYNTH_RULE`] — confirmed by ADR-0022's gate.
    /// A1 allows none.
    pub synth_confirmed: Vec<String>,
    /// Every decision.
    pub decisions: Vec<Decision>,
}

/// One negative job to run: a sub-population's scene.
pub struct Scene {
    /// Its sub-population.
    pub sub: &'static SubPop,
    /// The seed (`None` for the real window).
    pub seed: Option<u64>,
    /// The truth-stripped recording the mock device serves. `None` for a stub.
    pub meta: Option<std::path::PathBuf>,
}

/// The product through the mock SDR, or a stub that stands in for it.
pub trait Engine: Sync {
    /// Its name, for the report.
    fn name(&self) -> &'static str;
    /// Runs one scene at every profile in [`PROFILES`].
    fn run(&self, scene: &Scene) -> Run;
}

// ------------------------------------------------------------------------------------------
// The product.
// ------------------------------------------------------------------------------------------

/// **The product**: the scene's recording, truth stripped, through the mock SDR and the whole
/// pipeline with the IQ ring on; then, over the API `hk serve` exposes, one region-analyze job per
/// blind-detected emitter per profile, polled to its end.
pub struct Product;

/// Reads the inventory (all relations, all pages) as `(id, lifecycle actor)` pairs.
fn inventory(addr: std::net::SocketAddr) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    let mut path = "/api/inventory?limit=500&relations=all".to_owned();
    loop {
        let (status, body) = api_get(addr, &path);
        assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
        let v: Value = serde_json::from_slice(&body).unwrap();
        for r in v["entries"].as_array().cloned().unwrap_or_default() {
            out.push((
                r["id"].as_str().expect("a row has an id").to_owned(),
                r["lifecycle"]["actor"].as_str().map(str::to_owned),
            ));
        }
        match v.get("next_cursor").and_then(Value::as_u64) {
            Some(c) => path = format!("/api/inventory?limit=500&relations=all&cursor={c}"),
            None => break,
        }
    }
    out
}

/// The rank-1 result's hold-out arithmetic, from the served `AnalyzeJob` (docs/api.md). Reads the
/// hold-out block when the result carries one and falls back to the check summary, so a result
/// that reports only one of them still reaches A2.
fn winner(job: &Value) -> Option<Winner> {
    let r = job["results"].as_array()?.first()?;
    let bits = r["analytic_holdout_bits"].as_f64()?;
    let h = &r["holdout"];
    let c = &r["check"];
    Some(Winner {
        bits,
        check: c["model"]
            .as_str()
            .unwrap_or_else(|| r["summary"].as_str().unwrap_or("?"))
            .to_owned(),
        width: h["check_width"].as_u64().or_else(|| c["width"].as_u64()),
        differences: h["differences"].as_u64(),
        l_check: h["l_check"].as_f64(),
        check_searched: h["check_origin"]
            .as_str()
            .map(|o| o != "template-fixed")
            .or_else(|| h["check_origin"].as_object().map(|_| true)),
        null_margin_bits: h["null_control"]["margin_bits"].as_f64(),
        null_capped: h["null_control"]["capped"].as_bool(),
    })
}

/// What a job is aimed at (ADR-0021 §8.4: "blind detection **or an ad-hoc band**", never a truth
/// frequency).
#[derive(Clone, Debug)]
enum Target {
    /// An emitter blind detection produced.
    Emitter(String),
    /// An ad-hoc band over a window the device actually sampled, with its time extent.
    Band {
        /// For the report.
        label: String,
        /// Hz.
        f_lo: f64,
        /// Hz.
        f_hi: f64,
        /// Unix s.
        t_lo: f64,
        /// Unix s.
        t_hi: f64,
    },
}

impl Target {
    /// The label a decision records.
    fn label(&self) -> String {
        match self {
            Self::Emitter(id) => id.clone(),
            Self::Band { label, .. } => label.clone(),
        }
    }
}

/// The ad-hoc band targets: one per retained IQ-ring segment, spanning **the window the device
/// reports it sampled** (`GET /api/iqbuffer`'s `segments[].center_hz` ± `sample_rate_hz / 2`, over
/// the segment's own `t0`..`t1`) — the ordinary client path over provenance the SDR configuration
/// wrote, not truth.
///
/// Without them the population A1 needs most makes **no confirm decision at all**: thermal noise
/// produces no emitter (T-568's N1 rule *requires* that), so an emitter-only suite would count
/// zero decisions over N1 for ever and read green by having asked nothing. This is docs/22's
/// "deep noise run" target.
fn band_targets(addr: std::net::SocketAddr) -> Vec<Target> {
    let (status, body) = api_get(addr, "/api/iqbuffer");
    assert_eq!(
        status,
        200,
        "[{USE_CASES}] GET /api/iqbuffer: {}",
        String::from_utf8_lossy(&body)
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    v["segments"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| {
            let centre = s["center_hz"].as_f64()?;
            let width = s["bandwidth_hz"]
                .as_f64()
                .or_else(|| s["sample_rate_hz"].as_f64())?;
            let (t_lo, t_hi) = (s["t0"].as_f64()?, s["t1"].as_f64()?);
            (width > 0.0 && t_hi > t_lo).then(|| Target::Band {
                label: format!(
                    "band@{:.4}MHz±{:.0}kHz (ring segment {})",
                    centre / 1e6,
                    width / 2e3,
                    s["id"]
                ),
                f_lo: centre - width / 2.0,
                f_hi: centre + width / 2.0,
                t_lo,
                t_hi,
            })
        })
        .collect()
}

/// Starts one region-analyze job over `target` at `profile` and reads the finished job.
/// Bounded by the profile's own `max_evaluations`; nothing here sets `max_wall_s` (ADR-0021 §8.4).
fn analyze(addr: std::net::SocketAddr, target: &Target, profile: &'static str) -> Value {
    let mut body = json!({ "profile": profile });
    match target {
        Target::Emitter(id) => body["emitter_id"] = json!(id),
        Target::Band {
            f_lo,
            f_hi,
            t_lo,
            t_hi,
            ..
        } => body["band"] = json!({ "f_lo": f_lo, "f_hi": f_hi, "t_lo": t_lo, "t_hi": t_hi }),
    }
    let (status, raw) = api_post(addr, "/api/analyze", &body.to_string());
    let v: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    assert_eq!(
        status,
        202,
        "[{USE_CASES}] POST /api/analyze {}: {}",
        body,
        String::from_utf8_lossy(&raw)
    );
    wait_job(addr, &v["job"])
}

impl Engine for Product {
    fn name(&self) -> &'static str {
        "product (mock SDR, blind, /api/analyze)"
    }

    fn run(&self, scene: &Scene) -> Run {
        let meta = scene
            .meta
            .as_ref()
            .expect("a product scene has a recording");
        let tag = format!("fc-{}-{}", scene.sub.level, scene.seed.unwrap_or(0));
        let mut cfg = blind_config(meta, &tag, BlindSource::default(), json!({}));
        // A region job reads only the IQ ring, so the ring is on. Configuration, not truth.
        cfg.cfg.iq_buffer = IqBufferConfig {
            enabled: Some(true),
            retention_s: 600.0,
            max_bytes: Some(64 << 20),
            min_free_bytes: Some(0),
            ..IqBufferConfig::default()
        };
        let dir = cfg.dir;
        let handle = start(cfg.cfg, cfg.replay);
        let ring = handle.iq_buffer();
        assert!(
            ring.wait_allocated(Duration::from_secs(120)),
            "[{USE_CASES}] the IQ ring did not finish opening"
        );
        let _ = finish(handle);
        let server = serve_with_analyze(&dir.0, Arc::clone(&ring));
        let addr = server.local_addr();
        let before = inventory(addr);
        let mut targets: Vec<Target> = before
            .iter()
            .map(|(id, _)| Target::Emitter(id.clone()))
            .collect();
        targets.extend(band_targets(addr));
        let mut decisions = Vec::new();
        for profile in PROFILES {
            for target in &targets {
                let job = analyze(addr, target, profile);
                decisions.push(Decision {
                    level: scene.sub.level,
                    pop: scene.sub.pop,
                    seed: scene.seed,
                    profile,
                    target: target.label(),
                    state: job["state"].as_str().unwrap_or("?").to_owned(),
                    error: job["error"]["code"].as_str().map(str::to_owned),
                    confirm_outcome: job["confirm"]["outcome"].as_str().map(str::to_owned),
                    budget_claim: job["confirm"]["decision_rate"]["budget_claim"]
                        .as_str()
                        .map(str::to_owned),
                    holdout: winner(&job),
                });
            }
        }
        let after = inventory(addr);
        Run {
            level: scene.sub.level,
            seed: scene.seed,
            fixture: meta.display().to_string(),
            emitters_before: before.len(),
            emitters_created: after
                .iter()
                .filter(|(id, _)| !before.iter().any(|(b, _)| b == id))
                .map(|(id, _)| id.clone())
                .collect(),
            synth_confirmed: after
                .iter()
                .filter(|(_, actor)| actor.as_deref() == Some(CONFIRM_SYNTH_RULE))
                .map(|(id, _)| id.clone())
                .collect(),
            decisions,
        }
    }
}

/// Builds the product's scenes: T-568's N1/N2/N4 sub-populations over [`CI_SEEDS`]' open seeds.
/// `None` when synthesis is unavailable (the suite-wide skip rule); a missing real fixture skips
/// that sub-population alone, and says so.
fn product_scenes() -> Option<Vec<Scene>> {
    use hk_e2e::SynthRequest;
    use hk_e2e::corpus::SeedPlan;

    let mut scenes = Vec::new();
    for sub in SUBPOPS.iter().filter(|s| A1_POPULATIONS.contains(&s.pop)) {
        match sub.source {
            Source::Synth { scenario, params } => {
                for seed in
                    SeedPlan::new(&crate::mauto_negatives::scene_id(sub), CI_SEEDS).runnable()
                {
                    let mut req = SynthRequest::new(scenario).seed(seed);
                    for (k, v) in params {
                        req = req.param(*k, v);
                    }
                    let out = match req.generate() {
                        Ok(out) => out,
                        Err(err) if err.is_unavailable() && !hk_e2e::synth::require_synth() => {
                            eprintln!("SKIP {USE_CASES}: {err}");
                            return None;
                        }
                        Err(err) => panic!("{scenario} seed {seed}: generation failed: {err}"),
                    };
                    scenes.push(Scene {
                        sub,
                        seed: Some(seed),
                        meta: Some(out.fixture(0).unwrap().meta_path.clone()),
                    });
                }
            }
            Source::Real { dir, name } => match real_fixture_in(dir, name) {
                Some(meta) => scenes.push(Scene {
                    sub,
                    seed: None,
                    meta: Some(meta),
                }),
                None => eprintln!(
                    "SKIP {USE_CASES}: {} ({name}) is not fetched; N4 did not run",
                    sub.level
                ),
            },
        }
    }
    Some(scenes)
}

/// Runs every scene, a few at a time (each is a whole pipeline run plus its jobs).
fn run_all(engine: &dyn Engine, scenes: &[Scene]) -> Vec<Run> {
    const PARALLEL: usize = 3;
    let mut out = Vec::with_capacity(scenes.len());
    for chunk in scenes.chunks(PARALLEL) {
        out.extend(std::thread::scope(|s| {
            let hs: Vec<_> = chunk.iter().map(|c| s.spawn(|| engine.run(c))).collect();
            hs.into_iter()
                .map(|h| h.join().expect("a scene panicked"))
                .collect::<Vec<_>>()
        }));
    }
    out
}

// ------------------------------------------------------------------------------------------
// A1 and A2, as pure functions over the runs.
// ------------------------------------------------------------------------------------------

/// A1's counts (ADR-0022 §10.1), per profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A1 {
    /// Decisions made.
    pub decisions: usize,
    /// Decisions where a search actually ran.
    pub searched: usize,
    /// Confirms. The budget is 0.
    pub confirms: usize,
    /// Emitters the analyze/attach path created. The budget is 0.
    pub emitters_created: usize,
    /// Rows confirmed by [`CONFIRM_SYNTH_RULE`]. The budget is 0.
    pub synth_confirmed: usize,
}

/// A2's measurement (ADR-0022 §10.1), over every rank-1 hold-out result on input with nothing to
/// find.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct A2 {
    /// Negative decisions the tail is located from — searched decisions, not jobs.
    pub n: usize,
    /// Rank-1 hold-out results observed (≤ `n`).
    pub results: usize,
    /// Every observed `analytic_holdout_bits`, descending.
    pub bits: Vec<f64>,
    /// The largest, with its arithmetic and where it came from.
    pub max: Option<(f64, String)>,
    /// The fitted upper-tail slope and its ±1.96σ half-width — **reported, never asserted**
    /// (docs/22 §3 item 3: there is no a-priori slope to fix).
    pub slope: Option<(f64, f64)>,
}

/// `q` (0..=1) quantile of a descending list, nearest-rank. `None` when empty.
pub fn quantile(desc: &[f64], q: f64) -> Option<f64> {
    if desc.is_empty() {
        return None;
    }
    // desc[0] is the maximum, i.e. the 1.0 quantile.
    let idx = ((1.0 - q) * (desc.len() as f64 - 1.0)).round() as usize;
    desc.get(idx.min(desc.len() - 1)).copied()
}

/// The upper-tail slope: least squares of `log₂(exceedance)` against claimed bits over the **top
/// decade of exceedance** (ranks 1…10, i.e. `1/n`…`10/n`), with the ±1.96σ half-width of the
/// slope. `None` below 4 points or with no spread in bits. A slope of −1 is the nominal model; −0.6
/// says the tail is far fatter than log-linear and the extrapolation from `log₂(n)` bits to the
/// 24-bit gate is worth much less than the maximum suggests.
pub fn tail_slope(desc: &[f64], n: usize) -> Option<(f64, f64)> {
    let k = desc.len().min(10);
    if k < 4 || n == 0 {
        return None;
    }
    let pts: Vec<(f64, f64)> = desc[..k]
        .iter()
        .enumerate()
        .map(|(i, &b)| (b, ((i + 1) as f64 / n as f64).log2()))
        .collect();
    let m = pts.len() as f64;
    let mx = pts.iter().map(|p| p.0).sum::<f64>() / m;
    let my = pts.iter().map(|p| p.1).sum::<f64>() / m;
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    if sxx <= f64::EPSILON {
        return None;
    }
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let slope = sxy / sxx;
    let resid: f64 = pts
        .iter()
        .map(|p| (p.1 - (my + slope * (p.0 - mx))).powi(2))
        .sum();
    let se = if m > 2.0 {
        (resid / (m - 2.0) / sxx).sqrt()
    } else {
        f64::NAN
    };
    Some((slope, 1.96 * se))
}

/// A1 per profile and A2 pooled, from the runs. Pure.
pub fn assess(runs: &[Run]) -> (BTreeMap<&'static str, A1>, A2) {
    let mut a1: BTreeMap<&'static str, A1> = BTreeMap::new();
    let mut bits: Vec<(f64, String)> = Vec::new();
    let mut n = 0usize;
    for run in runs {
        for d in &run.decisions {
            let e = a1.entry(d.profile).or_default();
            e.decisions += 1;
            if d.searched() {
                e.searched += 1;
                n += 1;
            }
            if d.confirmed() {
                e.confirms += 1;
            }
            if let Some(w) = &d.holdout {
                bits.push((w.bits, describe(run, d, w)));
            }
        }
        // Inventory effects are per scene, and a scene's jobs span every profile: charge them to
        // each profile rather than guessing which job did it. A1's budget is 0 either way.
        for e in a1.values_mut() {
            e.emitters_created += run.emitters_created.len();
            e.synth_confirmed += run.synth_confirmed.len();
        }
    }
    bits.sort_by(|a, b| b.0.total_cmp(&a.0));
    let desc: Vec<f64> = bits.iter().map(|b| b.0).collect();
    let a2 = A2 {
        n,
        results: bits.len(),
        max: bits.first().cloned(),
        slope: tail_slope(&desc, n),
        bits: desc,
    };
    (a1, a2)
}

/// One observation, in the words ADR-0022 §10.2's message uses.
fn describe(run: &Run, d: &Decision, w: &Winner) -> String {
    let f = |x: Option<f64>| x.map_or("?".to_owned(), |v| format!("{v:.1}"));
    format!(
        "{} seed {:?}, {} profile, target {}: check {} {}, width {}, differences {}, L_check {}, \
         null control margin {} bits ({}); replay: just replay {}",
        run.level,
        d.seed,
        d.profile,
        d.target,
        w.check,
        match w.check_searched {
            Some(true) => "searched",
            Some(false) => "template-fixed",
            None => "origin unrecorded",
        },
        w.width.map_or("?".to_owned(), |v| v.to_string()),
        w.differences.map_or("?".to_owned(), |v| v.to_string()),
        f(w.l_check),
        f(w.null_margin_bits),
        match w.null_capped {
            Some(true) => "CAPPED",
            Some(false) => "did not cap",
            None => "did not run",
        },
        run.fixture,
    )
}

/// A1's failures, each with its evidence. Empty = A1 held.
pub fn a1_failures(a1: &BTreeMap<&'static str, A1>, runs: &[Run]) -> Vec<String> {
    let mut out = Vec::new();
    for (profile, x) in a1 {
        if x.confirms > 0 {
            out.push(format!(
                "A1 ({profile}): {} confirm(s) on input with nothing to find, over {} decision(s); \
                 the budget is 0. Confirmed: {}",
                x.confirms,
                x.decisions,
                runs.iter()
                    .flat_map(|r| r.decisions.iter().map(move |d| (r, d)))
                    .filter(|(_, d)| d.profile == *profile && d.confirmed())
                    .map(|(r, d)| format!(
                        "{} seed {:?} target {} (replay: just replay {})",
                        d.level, d.seed, d.target, r.fixture
                    ))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        if x.synth_confirmed > 0 {
            out.push(format!(
                "A1 ({profile}): {} inventory row(s) confirmed by {CONFIRM_SYNTH_RULE} on \
                 negatives: {:?}",
                x.synth_confirmed,
                runs.iter()
                    .flat_map(|r| r.synth_confirmed.iter())
                    .collect::<Vec<_>>(),
            ));
        }
        if x.emitters_created > 0 {
            out.push(format!(
                "A1 ({profile}): the analyze/attach path created {} emitter(s) on negatives: {:?}",
                x.emitters_created,
                runs.iter()
                    .flat_map(|r| r.emitters_created.iter())
                    .collect::<Vec<_>>(),
            ));
        }
    }
    out
}

/// ADR-0022 §10.2's **legible** A2 failure, or `None` when A2 held (or has no data). It states the
/// budget and its units, the threshold with its two terms, the observation, the implied margin,
/// what that means in wrong rows per week, the winning result's arithmetic and the replay command
/// — because `assert!(max <= 18.0)` tells a reader none of it.
///
/// The arithmetic of the implication, stated so it can be checked: an observed maximum of `b` at
/// the `1/n` quantile leaves a **realised margin** of `24 − b` bits where `9.7` were assumed, so
/// the nulls are optimistic by `2^(9.7 − (24 − b))` **more than assumed**, and since the threshold
/// was set to deliver exactly one wrong row per week, that factor *is* the wrong rows per week.
/// (ADR-0022 §10.2's illustrative block prints "~2.4 wrong Confirmed emitters per week" beside
/// "missed by ~137x"; the two do not follow from each other. The factor is what is derived, so it
/// is what this message prints, with the chain shown.)
pub fn a2_failure(a2: &A2) -> Option<String> {
    let (bits, where_) = a2.max.clone()?;
    let limit = a2_threshold_bits(a2.n)?;
    if bits <= limit {
        return None;
    }
    let realised = THRESHOLD_BITS - bits;
    let excess = ASSUMED_MARGIN_BITS - realised;
    let factor = 2f64.powf(excess);
    let nominal_weeks = 1.0 / (2f64.powf(-THRESHOLD_BITS) * DECISIONS_PER_WEEK);
    let p50 = quantile(&a2.bits, 0.5).unwrap_or(f64::NAN);
    let p99 = quantile(&a2.bits, 0.99).unwrap_or(f64::NAN);
    Some(format!(
        "acceptance_mauto::false_confirm_budget  FAILED  (A2)\n\n  \
         budget            1 wrong Confirmed emitter / device-week at {DECISIONS_PER_WEEK:.0} \
         decisions/week\n                    = {BUDGET_PER_DECISION:.1e} per decision = \
         {BUDGET_BITS} bits          [ADR-0022 §1]\n  \
         threshold         {THRESHOLD_BITS:.1} analytic hold-out bits\n                    \
         = {BUDGET_BITS} budget + {ASSUMED_MARGIN_BITS} assumed model margin   [ADR-0022 §3]\n  \
         observed          max analytic hold-out bits on {} negative decision(s): {bits:.1}\n      \
         ({p50:.1} p50, {p99:.1} p99, {} rank-1 hold-out result(s))\n                    \
         -> realised margin {realised:.1} bits, not the assumed {ASSUMED_MARGIN_BITS}\n           \
         -> the analytic nulls are optimistic by >= 2^{excess:.1} = {factor:.0}x more\n            \
         than ADR-0022 §3.2 assumed\n  \
         implication       at this margin the budget is missed by ~{factor:.0}x:\n                 \
         ~{factor:.1} wrong Confirmed emitters per week, not 1 per {nominal_weeks:.0} weeks\n  \
         assertion         max <= log2(n) + {A2_FIRES_ABOVE_BITS} = {limit:.1} bits at n = {} \
         (docs/22 §3; ADR-0022's {A2_CONSTANT_BITS} is this at n = {A2_FROZEN_N})\n  \
         the observation   {where_}\n  \
         tail slope        {}\n",
        a2.n,
        a2.results,
        a2.n,
        slope_text(a2),
    ))
}

/// The tail slope as the report prints it — reported, never asserted.
fn slope_text(a2: &A2) -> String {
    match a2.slope {
        Some((s, hw)) => format!(
            "log2(exceedance) vs claimed bits over the top decade: {s:.2} +/- {hw:.2} \
             (nominal -1.00; flatter than -1 = a fatter tail, so extrapolating from log2(n) = \
             {:.1} bits to the {THRESHOLD_BITS:.0}-bit gate is worth less than the max suggests)",
            (a2.n.max(1) as f64).log2()
        ),
        None => "not fitted (fewer than 4 rank-1 hold-out results in the top decade)".to_owned(),
    }
}

/// A2's assertion. Empty = held or not measured; the report says which.
pub fn a2_failures(a2: &A2) -> Vec<String> {
    a2_failure(a2).into_iter().collect()
}

/// ADR-0022 §10.2's honest-bound paragraph, at the `n` that actually ran. This is the only thing
/// A1 may be quoted as showing.
pub fn honest_bound(n: usize) -> String {
    if n == 0 {
        return format!(
            "  A1 bounds NOTHING on this run: 0 searched negative decisions (no production search \
             backend), so the rule of three has no n. The budget is {BUDGET_PER_DECISION:.1e} per \
             decision; demonstrating it from a zero-failure run at 95 % needs n ~ 60 000. \
             {N60K_POSITION}"
        );
    }
    let bound = 3.0 / n as f64;
    format!(
        "  A1 honestly bounds this much and no more: 0 confirms in n = {n} negative decision(s) \
         bounds the true per-decision false-confirm rate to <= 3/n = {bound:.2e} at 95 % \
         confidence (the rule of three), against a budget of {BUDGET_PER_DECISION:.1e}. A1 IS \
         {:.0}x TOO WEAK TO DEMONSTRATE THE BUDGET and must never be quoted as doing so -- it can \
         only fail to contradict it. (At ADR-0022 §10.2's n = 800 per profile the factor is 75x.) \
         Demonstrating {BUDGET_PER_DECISION:.1e} from a zero-failure run at 95 % needs n ~ 60 000. \
         {N60K_POSITION}",
        bound / BUDGET_PER_DECISION,
    )
}

/// The report: A1 per profile, A2's distribution per population per profile, the tail slope, the
/// honest bound, and what was not measured.
pub fn report(engine: &str, runs: &[Run], a1: &BTreeMap<&'static str, A1>, a2: &A2) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "[{USE_CASES}] false-confirm budget, engine: {engine}");
    let _ = writeln!(
        s,
        "  A1  {:<10} {:>9} {:>8} {:>8} {:>9} {:>14}",
        "profile", "decisions", "searched", "confirms", "created", "synth-confirmed"
    );
    for (profile, x) in a1 {
        let _ = writeln!(
            s,
            "      {profile:<10} {:>9} {:>8} {:>8} {:>9} {:>14}",
            x.decisions, x.searched, x.confirms, x.emitters_created, x.synth_confirmed
        );
    }
    // A2's distribution per population per profile (ADR-0022 §10.1: "report p50, p99, max, per
    // population and per profile").
    let _ = writeln!(
        s,
        "  A2  {:<14} {:<10} {:>9} {:>8} {:>7} {:>7} {:>7}",
        "population", "profile", "decisions", "results", "p50", "p99", "max"
    );
    for pop in A1_POPULATIONS {
        for profile in PROFILES {
            let mut desc: Vec<f64> = runs
                .iter()
                .flat_map(|r| r.decisions.iter())
                .filter(|d| d.pop == *pop && d.profile == *profile)
                .filter_map(|d| d.holdout.as_ref().map(|w| w.bits))
                .collect();
            desc.sort_by(|a, b| b.total_cmp(a));
            let n = runs
                .iter()
                .flat_map(|r| r.decisions.iter())
                .filter(|d| d.pop == *pop && d.profile == *profile && d.searched())
                .count();
            let q = |v: Option<f64>| v.map_or("-".to_owned(), |x| format!("{x:.1}"));
            let _ = writeln!(
                s,
                "      {:<14} {profile:<10} {n:>9} {:>8} {:>7} {:>7} {:>7}",
                format!("{pop:?}"),
                desc.len(),
                q(quantile(&desc, 0.5)),
                q(quantile(&desc, 0.99)),
                q(desc.first().copied()),
            );
        }
    }
    let _ = match (a2.max.as_ref(), a2_threshold_bits(a2.n)) {
        (Some((b, w)), Some(limit)) => writeln!(
            s,
            "  A2  pooled max {b:.1} bits against log2({}) + {A2_FIRES_ABOVE_BITS} = {limit:.1} \
             (ADR-0022's {A2_CONSTANT_BITS} is this at n = {A2_FROZEN_N})\n      winner: {w}",
            a2.n
        ),
        _ => writeln!(
            s,
            "  A2  NOT MEASURED: {} rank-1 hold-out result(s) over {} searched negative \
             decision(s). Nothing implements the engine's Evaluator over acquired IQ \
             (hk_pipeline::synth::jobs::server_backend() is None), so every job ended \
             not-searched and there is no analytic-null tail to locate. A2 is ARMED: the moment a \
             backend searches, the same assertion binds with no edit here.",
            a2.results, a2.n
        ),
    };
    let _ = writeln!(s, "  A2  tail slope: {}", slope_text(a2));
    let _ = writeln!(s, "{}", honest_bound(a2.n));
    let _ = writeln!(
        s,
        "  A3  is the gate-level recall control (false_confirm_recall_control_*). Engine-side, \
         NOT measured here:"
    );
    for (row, what) in ENGINE_SIDE_ROWS {
        let _ = writeln!(s, "      - {row}: {what}");
    }
    let _ = writeln!(
        s,
        "  N4: the 50-ohm terminator row is ABSENT (T-375, a user action) -- the receiver-only \
         null is UNMEASURED (docs/22 §10)."
    );
    s
}

// ------------------------------------------------------------------------------------------
// The tests: A1 + A2 through the product.
// ------------------------------------------------------------------------------------------

/// **A1 and A2** over T-568's N1 ∪ N2 ∪ N4, blind through the mock SDR, at `standard` and `deep`.
#[test]
fn false_confirm_budget() {
    let Some(scenes) = product_scenes() else {
        return;
    };
    assert!(
        !scenes.is_empty(),
        "[{USE_CASES}] no negative scene ran -- an empty population passes every budget"
    );
    let runs = run_all(&Product, &scenes);
    let (a1, a2) = assess(&runs);
    eprintln!("{}", report(Product.name(), &runs, &a1, &a2));
    for run in &runs {
        for d in &run.decisions {
            eprintln!(
                "  [{} seed {:?} {}] target {} state {} error {:?} confirm {:?} claim {:?} bits {:?}",
                d.level,
                d.seed,
                d.profile,
                d.target,
                d.state,
                d.error,
                d.confirm_outcome,
                d.budget_claim,
                d.holdout.as_ref().map(|w| w.bits),
            );
        }
    }
    // Every population that has IQ ran, at every profile: a population that silently ran nothing
    // would pass A1 by doing nothing.
    for pop in A1_POPULATIONS {
        let ran = SUBPOPS
            .iter()
            .filter(|s| s.pop == *pop && matches!(s.source, Source::Synth { .. }))
            .all(|s| runs.iter().any(|r| r.level == s.level));
        assert!(
            ran,
            "[{USE_CASES}] {pop:?} did not run every synthetic sub-population"
        );
    }
    // …and every population made at least one confirm decision at every profile. N1 produces no
    // emitter by design (T-568's rule), so without the ad-hoc band target it would contribute
    // nothing and A1 would read green by having asked nothing.
    for profile in PROFILES {
        assert!(
            a1.contains_key(profile),
            "[{USE_CASES}] no decision at profile {profile}"
        );
        for pop in A1_POPULATIONS {
            let n = runs
                .iter()
                .flat_map(|r| r.decisions.iter())
                .filter(|d| d.pop == *pop && d.profile == *profile)
                .count();
            assert!(
                n > 0
                    || !runs
                        .iter()
                        .any(|r| r.decisions.iter().any(|d| d.pop == *pop))
                        && SUBPOPS.iter().filter(|s| s.pop == *pop).all(|s| {
                            matches!(s.source, Source::Real { .. })
                                && !runs.iter().any(|r| r.level == s.level)
                        }),
                "[{USE_CASES}] {pop:?} made no confirm decision at {profile}: a population \
                 nothing was asked about passes A1 by doing nothing"
            );
        }
    }
    // Every job really reached the pipeline: a refused POST is a plumbing defect, not a negative
    // result, and would otherwise read as "0 confirms".
    for run in &runs {
        for d in &run.decisions {
            assert!(
                !d.state.starts_with("refused"),
                "[{USE_CASES}] POST /api/analyze was refused for {} at {}: {}",
                d.target,
                d.profile,
                d.state
            );
        }
    }
    let mut fails = a1_failures(&a1, &runs);
    fails.extend(a2_failures(&a2));
    assert!(
        fails.is_empty(),
        "[{USE_CASES}] the false-confirm budget FAILED:\n{}",
        fails.join("\n")
    );
}

// ------------------------------------------------------------------------------------------
// A3: the recall control over the shipped gate.
// ------------------------------------------------------------------------------------------

/// A3's cases run against the **shipped** `SynthesizedConfirm`, built as ADR-0022 §4.2's table
/// describes them. Each case is stated as its arithmetic, and each carries the *paired* negative
/// that must refuse, so the control can fail.
mod recall {
    use hk_synth::result::{CheckOrigin, CheckSummary, HoldoutEvidence, PipelineResult};
    use hk_synth::trace::NullControl;
    use hk_synth::{Profile, Stage, Verdict};

    use super::*;

    /// A solved rank-1 result carrying one check, `differences` differing frames wide `width`,
    /// with `l_check` charged and `sync` bits of sync excess beside the check.
    pub fn solved(
        model: &str,
        width: u32,
        differences: u32,
        origin: CheckOrigin,
        l_check: f32,
        sync: f32,
    ) -> PipelineResult {
        let check_bits = width as f32 * differences as f32 - l_check;
        PipelineResult {
            rank: 1,
            verdict: Verdict::Solved,
            summary: format!("{model} x{differences}"),
            recipe: serde_json::from_str(include_str!("../../../../recipes/adsb.recipe.json"))
                .expect("the ADS-B recipe parses"),
            template: None,
            stage_reached: Stage::S5,
            stages: Vec::new(),
            evidence_bits: check_bits + sync,
            prior_bits: 0.0,
            analytic_holdout_bits: Some(check_bits + sync),
            check: Some(CheckSummary {
                kind: "crc".into(),
                model: model.into(),
                width,
                pass_rate: 1.0,
                // The repeated-payload case is exactly `distinct_valid > differences`.
                distinct_valid: differences,
                corrected_excluded: 0,
                tested: differences,
                holdout: true,
                node: None,
            }),
            frames_preview: Vec::new(),
            characterisation: None,
            holdout: Some(HoldoutEvidence {
                evidence_bits: check_bits + sync,
                analytic_bits: check_bits + sync,
                check_bits: Some(check_bits),
                l_check: Some(l_check),
                check_width: Some(width),
                differences,
                check_origin: origin,
                stages: Vec::new(),
                null_control: origin.searched().then_some(NullControl {
                    k: 2,
                    ran: true,
                    best_null_bits: check_bits + sync - 11.4,
                    margin_bits: 11.4,
                    capped: false,
                }),
            }),
        }
    }

    /// The gate, over a window whose front-end trust was measured and clean.
    pub fn decide(r: &PipelineResult, profile: Profile) -> Result<String, String> {
        SynthesizedConfirm::default().decide(&hk_pipeline::inventory::SynthesizedEvidence {
            profile,
            result: r,
            pipeline: "generic-fsk-framed".into(),
            suspect_fraction: Some(0.0),
            overload: Some(false),
        })
    }
}

/// **A3 (a)**: at least one ADS-B single squitter confirms **on one frame** — ADR-0022 §4.2's case
/// the old `min_distinct_valid = 3` rule blocked outright. A template-fixed CRC-24 carries 24
/// analytic bits in one frame; the same frame with the generator *searched* does not, because the
/// search's own multiplicity is charged.
#[test]
fn false_confirm_recall_control_an_adsb_single_squitter_confirms_on_one_frame() {
    use hk_synth::Profile;
    use hk_synth::result::CheckOrigin;

    let one = recall::solved("CRC-24", 24, 1, CheckOrigin::TemplateFixed, 0.0, 6.0);
    let reason = recall::decide(&one, Profile::Standard).expect("one template-fixed CRC-24 frame");
    assert!(
        reason.contains("CRC-24 (template-fixed), 1 differing frame"),
        "{reason}"
    );
    assert!(reason.contains("24.0 − 0.0 = 24.0 check bits"), "{reason}");
    eprintln!("[{USE_CASES}] A3(a) one squitter: {reason}");
    // The paired negative: the same single frame from a *searched* generator. ADR-0022 §5.2's
    // discount is L_check and nothing else, and one frame no longer pays for itself.
    let searched = recall::solved("CRC-24", 24, 1, CheckOrigin::Searched, 29.0, 6.0);
    let err = recall::decide(&searched, Profile::Standard).unwrap_err();
    assert!(err.contains("hard check floor"), "{err}");
    // And `quick` never confirms, whatever the evidence (ADR-0022 §6 step 1).
    assert!(
        recall::decide(&one, Profile::Quick)
            .unwrap_err()
            .contains("`quick` never confirms")
    );
}

/// **A3 (b)**: the CRC-8 row of ADR-0022 §4.2's table — "confirms at 3 differing frames", the case
/// the old flat `width ≥ 16` refused outright.
///
/// **It does not confirm on this build, and that is the shipped policy, not a defect here.**
/// T-577 *measured* the width floor and it moved **up**: ADR-0022 §4.3.1's hole (B) makes one 2⁻ʷ
/// event clear the gate at every width ≥ 8 through zero-padded shifts, so `min_check_width = 16`
/// until §4.3.1's count is re-measured by T-577's harness, and `effective()` clamps a configured 8
/// back to 16 (configuration may only tighten). So this test asserts the honest pair:
///
/// 1. the §4.2 *arithmetic* for CRC-8 is exactly the table's — `min_differences(8, 0) == 3`, and
///    three 8-bit frames carry 24 bits, which clears both the hard check floor and the 24-bit
///    threshold; and
/// 2. the **only** clause refusing it is the measured width floor, which the refusal names.
///
/// The day §4.3.1's count returns the floor to 8, clause 2 flips and this test says so in its own
/// failure message rather than silently passing on a weaker claim.
#[test]
fn false_confirm_recall_control_a_template_fixed_crc8_at_three_frames_is_refused_only_by_the_measured_width_floor()
 {
    use hk_synth::Profile;
    use hk_synth::result::CheckOrigin;

    let p = SynthesizedConfirm::default();
    assert_eq!(
        p.min_differences(8, 0.0),
        3,
        "ADR-0022 §4.2: a template-fixed CRC-8 needs 3 differing frames"
    );
    let three = recall::solved("CRC-8", 8, 3, CheckOrigin::TemplateFixed, 0.0, 6.0);
    let err = recall::decide(&three, Profile::Standard).unwrap_err();
    assert!(
        err.contains("8-bit check is under the 16-bit floor"),
        "the CRC-8 row must be refused by the width floor and by nothing else; it said: {err}"
    );
    eprintln!(
        "[{USE_CASES}] A3(b) CRC-8 x3 = 24 bits, {} needed: refused by T-577's measured floor \
         ({} bits). {err}",
        p.min_differences(8, 0.0),
        p.min_check_width,
    );
    // Its bits genuinely reach the gate: the same three frames one width up confirm, so what
    // refused CRC-8 is the floor and not the accounting.
    let reason = recall::decide(
        &recall::solved("CRC-16", 16, 2, CheckOrigin::TemplateFixed, 0.0, 6.0),
        Profile::Standard,
    )
    .expect("ADR-0022 §4.2's template-fixed CRC-16 row: 2 differing frames");
    assert!(reason.contains("2 differing frame(s)"), "{reason}");
    // And one frame of it does not: the count is a formula, not a constant.
    assert!(
        recall::decide(
            &recall::solved("CRC-16", 16, 1, CheckOrigin::TemplateFixed, 0.0, 6.0),
            Profile::Standard,
        )
        .is_err()
    );
}

/// **A3 (c)**: a repeated-payload beacon does **not** confirm on repeat count alone —
/// `differences`, not `distinct_valid` (ADR-0022 §4.2). Eight identical frames are one chance
/// event; eight differing ones are eight.
#[test]
fn false_confirm_recall_control_a_repeated_payload_beacon_does_not_confirm_on_repeat_count() {
    use hk_synth::Profile;
    use hk_synth::result::CheckOrigin;

    // What the old rule saw: 8 valid frames, `distinct_valid` 8, 128 "check bits".
    let mut beacon = recall::solved("CRC-16", 16, 8, CheckOrigin::TemplateFixed, 0.0, 6.0);
    let h = beacon.holdout.as_mut().unwrap();
    h.differences = 1; // the same payload every frame: one difference
    let err = recall::decide(&beacon, Profile::Standard).unwrap_err();
    assert!(
        err.contains("22.0 analytic hold-out bits against a 24-bit threshold"),
        "a beacon repeating one payload must not confirm on its repeat count: {err}"
    );
    assert_eq!(
        beacon.check.as_ref().unwrap().distinct_valid,
        8,
        "the over-crediting count is still reported -- the gate simply does not read it"
    );
    eprintln!("[{USE_CASES}] A3(c) repeated payload (distinct_valid 8, differences 1): {err}");
    // Eight *differing* payloads: the same frames, and they confirm.
    beacon.holdout.as_mut().unwrap().differences = 8;
    assert!(recall::decide(&beacon, Profile::Standard).is_ok());
}

// ------------------------------------------------------------------------------------------
// The suite must be able to fail (docs/22 §6.2), and its arithmetic must be checkable.
// ------------------------------------------------------------------------------------------

/// A stub decision, for the pure assertions.
fn stub_run(level: &'static str, pop: Population, decisions: Vec<Decision>) -> Run {
    Run {
        level,
        seed: Some(1),
        fixture: format!("fixtures/negatives/{level}-1.sigmf-meta"),
        emitters_before: 1,
        emitters_created: Vec::new(),
        synth_confirmed: Vec::new(),
        decisions: decisions
            .into_iter()
            .map(|d| Decision { level, pop, ..d })
            .collect(),
    }
}

/// A `done` decision at `profile` carrying `bits` analytic hold-out bits.
fn stub_decision(profile: &'static str, bits: Option<f64>, confirmed: bool) -> Decision {
    Decision {
        level: "stub",
        pop: Population::N2,
        seed: Some(1),
        profile,
        target: "e-stub".into(),
        state: "done".into(),
        error: None,
        confirm_outcome: Some(
            if confirmed {
                "confirmed"
            } else {
                "insufficient"
            }
            .into(),
        ),
        budget_claim: Some("holds".into()),
        holdout: bits.map(|bits| Winner {
            bits,
            check: "CRC-16".into(),
            width: Some(16),
            differences: Some(3),
            l_check: Some(20.4),
            check_searched: Some(true),
            null_margin_bits: Some(9.2),
            null_capped: Some(false),
        }),
    }
}

/// **The degeneracy detector for A1**: a single confirm on input with nothing to find turns the
/// suite red, and the failure names the fixture to replay.
#[test]
fn false_confirm_budget_can_fail_one_confirm_on_a_negative_turns_the_suite_red() {
    let runs = vec![stub_run(
        "n2-cw",
        Population::N2,
        vec![
            stub_decision("standard", None, true),
            stub_decision("deep", None, false),
        ],
    )];
    let (a1, a2) = assess(&runs);
    let fails = a1_failures(&a1, &runs);
    assert!(!fails.is_empty(), "A1 cannot fail");
    assert!(
        fails[0].contains("A1 (standard): 1 confirm(s)") && fails[0].contains("just replay"),
        "{fails:#?}"
    );
    assert!(a2_failures(&a2).is_empty(), "no bits were observed");
    // A created emitter and a synth-confirmed row each fail on their own.
    let mut with_row = runs.clone();
    with_row[0].synth_confirmed = vec!["e-9".into()];
    with_row[0].emitters_created = vec!["e-9".into()];
    let (a1b, _) = assess(&with_row);
    let f = a1_failures(&a1b, &with_row).join("\n");
    assert!(f.contains(CONFIRM_SYNTH_RULE), "{f}");
    // Charged to each profile: a scene's jobs span every profile, and attributing an inventory
    // effect to one of them would be a guess. The budget is 0 either way.
    assert_eq!(f.matches("created 1 emitter(s)").count(), 2, "{f}");
}

/// **The degeneracy detector for A2**, and the whole point of the ticket: a tail that reaches 21.4
/// bits fails **while A1 still reads zero**, and the message is ADR-0022 §10.2's, not a bare
/// inequality.
#[test]
fn false_confirm_budget_can_fail_a_21_bit_tail_fails_a2_while_a1_reads_zero() {
    // 1 600 negative decisions (docs/22's frozen n) where nothing confirmed, one of which reached
    // 21.4 analytic bits — ADR-0022 §10.2's worked example.
    let mut decisions: Vec<Decision> = (0..A2_FROZEN_N)
        .map(|i| {
            let profile = if i % 2 == 0 { "standard" } else { "deep" };
            // A log-linear tail below the outlier: the i-th largest at 16.2 − log2(i) bits.
            let bits = 16.2 - ((i + 1) as f64).log2();
            stub_decision(profile, Some(bits), false)
        })
        .collect();
    decisions[0] = stub_decision("deep", Some(21.4), false);
    let runs = vec![stub_run("n2-cw", Population::N2, decisions)];
    let (a1, a2) = assess(&runs);
    assert_eq!(a2.n, A2_FROZEN_N);
    assert!(
        a1_failures(&a1, &runs).is_empty(),
        "A1 must still read zero -- that is why A2 exists"
    );
    let msg = a2_failure(&a2).expect("21.4 bits must fail A2 at n = 1600");
    eprintln!("{msg}");
    for want in [
        "acceptance_mauto::false_confirm_budget  FAILED  (A2)",
        "= 5.0e-5 per decision = 14.29 bits",
        "= 14.29 budget + 9.7 assumed model margin",
        "max analytic hold-out bits on 1600 negative decision(s): 21.4",
        "realised margin 2.6 bits, not the assumed 9.7",
        "optimistic by >= 2^7.1 = 137x more",
        "the budget is missed by ~137x",
        "not 1 per 839 weeks",
        "max <= log2(n) + 7.4 = 18.0 bits at n = 1600",
        "check CRC-16 searched, width 16, differences 3, L_check 20.4",
        "null control margin 9.2 bits (did not cap)",
        "replay: just replay fixtures/negatives/n2-cw-1.sigmf-meta",
        "tail slope",
    ] {
        assert!(
            msg.contains(want),
            "the message is missing {want:?}:\n{msg}"
        );
    }
    // ADR-0022's constant is exactly this assertion at docs/22's frozen n.
    assert!((a2_threshold_bits(A2_FROZEN_N).unwrap() - A2_CONSTANT_BITS).abs() < 0.05);
    // 18.0 bits held: the assertion is about where the tail is, not about any observation.
    let held: Vec<Decision> = (0..A2_FROZEN_N)
        .map(|i| stub_decision("standard", Some(16.2 - ((i + 1) as f64).log2()), false))
        .collect();
    let (_, ok) = assess(&[stub_run("n2-cw", Population::N2, held)]);
    assert!(a2_failure(&ok).is_none(), "16.2 bits at n = 1600 holds");
}

/// docs/22 §3 item 2: **the `18.0` is a function of `n` and must not be frozen independently of
/// it.** A suite that silently shrinks its negative population would otherwise be
/// indistinguishable from one that passes: at n = 200 the constant means "optimism ≤ 10.3 bits",
/// *above* the assumed margin, i.e. no power at all. Here the threshold shrinks with `n`, so a
/// smaller population is judged harder.
#[test]
fn a2_threshold_is_a_function_of_n_so_a_shrunken_population_cannot_pass_silently() {
    assert_eq!(a2_threshold_bits(0), None, "no decision, no tail");
    let at = |n: usize| a2_threshold_bits(n).unwrap();
    assert!((at(A2_FROZEN_N) - A2_CONSTANT_BITS).abs() < 0.05);
    assert!(at(200) < at(A2_FROZEN_N), "a smaller n is a tighter bound");
    assert!(at(60_000) > at(A2_FROZEN_N), "a larger n resolves deeper");
    // docs/22 §3's table, to the tenth of a bit. The observed maximum estimates the log2(n)-bit
    // quantile, so under optimism of `X` bits it sits at `log2(n) + X`, and
    //
    //   - the FIXED constant `max <= 18` therefore fires above `18 − log2(n)` — the table's
    //     column, which is exactly why the constant must not be frozen independently of `n`;
    //   - the SCALED threshold `log2(n) + 7.4` fires above 7.4 at EVERY n, which is the point of
    //     scaling it: the power is held constant (conservative by 2.3 bits against the assumed
    //     9.7) instead of quietly evaporating as the population shrinks.
    for (n, resolves, fires_above_fixed) in [
        (800usize, 9.6, 8.4),
        (A2_FROZEN_N, 10.6, 7.4),
        (20_000, 14.3, 3.7),
        (60_000, 15.9, 2.1),
    ] {
        let resolved = (n as f64).log2();
        assert!((resolved - resolves).abs() < 0.1, "n = {n}");
        assert!(
            (A2_CONSTANT_BITS - resolved - fires_above_fixed).abs() < 0.1,
            "n = {n}: the fixed 18.0 fires above {:.1}, docs/22 §3 says {fires_above_fixed}",
            A2_CONSTANT_BITS - resolved,
        );
        assert!(
            (at(n) - resolved - A2_FIRES_ABOVE_BITS).abs() < 1e-9,
            "n = {n}: the scaled threshold must hold the power at {A2_FIRES_ABOVE_BITS} bits"
        );
        // …and it complains before the budget breaks, at every n (docs/22 §3 item 1).
        const _: () = assert!(A2_FIRES_ABOVE_BITS < ASSUMED_MARGIN_BITS);
    }
    // The honest bound is stated at the n that ran, never at a nicer n, and never as a claim.
    let text = honest_bound(800);
    assert!(text.contains("3/n = 3.75e-3"), "{text}");
    assert!(text.contains("75x TOO WEAK"), "{text}");
    assert!(honest_bound(0).contains("bounds NOTHING"));
    assert!(honest_bound(0).contains("n ~ 60 000"));
}

/// docs/22 §3 item 3: **the extrapolation from `log₂(n)` bits to 24 must be visible, not hidden
/// inside a `max`.** The slope is reported with its interval and never asserted — a slope of −1 is
/// the nominal model; flatter than −1 is a fatter tail.
#[test]
fn false_confirm_budget_reports_the_upper_tail_slope_and_tells_a_fat_tail_from_a_nominal_one() {
    // bits_i = b0 − s·log2(i) gives an exceedance slope of −1/s.
    let sample = |s: f64| -> Vec<f64> {
        (0..64)
            .map(|i| 20.0 - s * ((i + 1) as f64).log2())
            .collect()
    };
    let (nominal, hw) = tail_slope(&sample(1.0), 1600).expect("a fit");
    assert!(
        (nominal + 1.0).abs() < 0.05 && hw < 0.05,
        "log-linear: {nominal:.2} +/- {hw:.2}"
    );
    let (fat, _) = tail_slope(&sample(1.0 / 0.6), 1600).expect("a fit");
    assert!((fat + 0.6).abs() < 0.05, "fat-tailed: {fat:.2}");
    assert!(fat > nominal, "a fatter tail reads flatter than -1");
    // Too few points to fit, and no spread: reported as not fitted, never as a number.
    assert_eq!(tail_slope(&[20.0, 19.0, 18.0], 1600), None);
    assert_eq!(tail_slope(&[20.0; 8], 1600), None);
    assert_eq!(tail_slope(&sample(1.0), 0), None);
    let (_, a2) = assess(&[stub_run(
        "n2-cw",
        Population::N2,
        sample(1.0)
            .into_iter()
            .map(|b| stub_decision("deep", Some(b), false))
            .collect(),
    )]);
    let text = slope_text(&a2);
    assert!(text.contains("nominal -1.00"), "{text}");
    assert!(
        report("stub", &[], &BTreeMap::new(), &a2).contains("tail slope"),
        "the slope is in the report"
    );
}

/// The report carries what ADR-0022 §10.2 and this ticket's DoD require, and — with nothing
/// searched — says so instead of implying a bound it does not have.
#[test]
fn false_confirm_budget_reports_the_honest_bound_and_the_n60000_position() {
    let runs = vec![stub_run(
        "n1-thermal",
        Population::N1,
        vec![Decision {
            state: "failed".into(),
            error: Some("no_evaluator".into()),
            confirm_outcome: None,
            budget_claim: None,
            holdout: None,
            ..stub_decision("standard", None, false)
        }],
    )];
    let (a1, a2) = assess(&runs);
    assert_eq!(a2.n, 0, "a failed job ruled nothing out");
    assert_eq!(a1["standard"].searched, 0);
    let text = report("stub", &runs, &a1, &a2);
    eprintln!("{text}");
    for want in [
        "A2  NOT MEASURED",
        "server_backend() is None",
        "A2 is ARMED",
        "A1 bounds NOTHING on this run",
        "n ~ 60 000",
        "WORTH BUILDING",
        "NOT YET",
        "Never in CI",
        "50-ohm terminator row is ABSENT",
        "ADS-B squitter scene, burst path",
    ] {
        assert!(text.contains(want), "the report is missing {want:?}");
    }
    // The report must never claim the budget was demonstrated.
    assert!(
        !text.contains("demonstrates"),
        "A1 can only fail to contradict the budget"
    );
}
