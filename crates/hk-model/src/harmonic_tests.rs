//! T-374: harmonic families, and the negative control that makes the mechanism mean something.
//!
//! **The positive control is T-317's own measurement**, reconstructed from the figures recorded in
//! `docs/planning-log.md` B0.654: harmonics 43, 44 and 45 of a free-running ~2.3364 MHz
//! oscillator, the lowest at the 100.465339 MHz box of `capture-2026-09-15-fm-band`, whose three
//! independent `f/n` estimates of `f₀` agree to 9 Hz, whose fitted intercept is +70 Hz, and whose
//! rms width ÷ n reads 138 / 120 / 137 Hz. The exact member frequencies below are the unique
//! triple that reproduces all three of those published figures at once; the derivation is written
//! out at [`t317`].
//!
//! **The negative control is the point of the ticket.** A fit over enough emitters will always
//! find some `f₀`, so the tests here run real unrelated emitters and 4000 randomly drawn
//! populations through the same search and require it to say no.

use crate::harmonic::*;
use crate::ids::EmitterId;
use crate::relate::ReceiveChain;

fn eid(n: u8) -> EmitterId {
    EmitterId::from_uuid(uuid::Uuid::from_bytes([n; 16]))
}

fn member(n: u8, f_hz: f64, width_hz: f64) -> FamilyMember {
    FamilyMember {
        emitter_id: eid(n),
        f_center_hz: f_hz,
        width_hz,
        chains: vec![ReceiveChain::device("hackrf-0")],
        line_shape: None,
    }
}

/// T-317's three members, reconstructed from the figures in `docs/planning-log.md` B0.654.
///
/// The log records three things about the same fit, and they are not independently choosable:
/// the n = 43 member sits at **100.465339 MHz**, the three per-member estimates `fᵢ/nᵢ` of `f₀`
/// **agree to 9 Hz**, and the least-squares intercept is **+67 Hz**. Writing `fᵢ = nᵢ·F + eᵢ`,
/// the intercept of a three-point fit at n = 43, 44, 45 is `b = 22⅓·e₄₃ + ⅓·e₄₄ − 21⅔·e₄₅` — the
/// long lever arm down to zero frequency — while the spread of `fᵢ/nᵢ` is the spread of `eᵢ/nᵢ`.
/// Nine hertz of the latter needs ~400 Hz of `e`, and `b` is then a near-cancellation of two
/// ~8.9 kHz terms. `e = (+400, 0, +409)` Hz reproduces both: spread 9.30 Hz (3.98 ppm), `b` = +71
/// Hz. The widths are `n` × the logged rms width ÷ n of 138 / 120 / 137 Hz.
fn t317() -> Vec<FamilyMember> {
    // F = (100_465_339 − 400) / 43
    let f = (100_465_339.0 - 400.0) / 43.0;
    vec![
        member(43, 100_465_339.0, 43.0 * 138.0),
        member(44, 44.0 * f, 44.0 * 120.0),
        member(45, 45.0 * f + 409.0, 45.0 * 137.0),
    ]
}

/// Real unrelated emitters on one receive chain: eight broadcast FM stations across a band, a
/// NOAA weather channel, an airband voice channel and a marine VHF channel. Nothing here is a
/// harmonic of anything here.
fn unrelated_real_emitters() -> Vec<FamilyMember> {
    vec![
        member(1, 88_600_000.0, 180_000.0),
        member(2, 91_300_000.0, 180_000.0),
        member(3, 95_800_000.0, 180_000.0),
        member(4, 97_700_000.0, 180_000.0),
        member(5, 99_692_500.0, 180_000.0),
        member(6, 101_100_000.0, 180_000.0),
        member(7, 103_400_000.0, 180_000.0),
        member(8, 107_100_000.0, 180_000.0),
        member(9, 162_550_000.0, 25_000.0),
        member(10, 118_700_000.0, 8_330.0),
        member(11, 156_800_000.0, 16_000.0),
    ]
}

// -------------------------------------------------------------------------------------------
// 1. The positive control
// -------------------------------------------------------------------------------------------

#[test]
fn t317_oscillator_family_is_found_blind_among_unrelated_stations() {
    // The population is T-317's three members mixed into the real unrelated stations above.
    // Nothing tells the search which three belong together.
    let mut pool = unrelated_real_emitters();
    pool.extend(t317());
    let found = find_harmonic_families(&pool);
    assert_eq!(found.len(), 1, "exactly one family: {found:#?}");
    let f = &found[0];
    let idx: Vec<u32> = f.members.iter().map(|m| m.index).collect();
    assert_eq!(idx, vec![43, 44, 45], "{}", f.arithmetic());
    assert!(
        (f.f0_hz - 2_336_398.0).abs() < 50.0,
        "fundamental {} Hz: {}",
        f.f0_hz,
        f.arithmetic()
    );
    // The three members are T-317's, and none of the real stations was swept in.
    let ids: Vec<EmitterId> = f.members.iter().map(|m| m.emitter_id).collect();
    assert_eq!(ids, vec![eid(43), eid(44), eid(45)]);
    eprintln!("T-317 family: {}", f.arithmetic());
}

#[test]
fn t317_numbers_match_what_the_capture_measured() {
    let fam = judge_family(&t317(), &[43, 44, 45]).expect("T-317 is a family");
    // The fit.
    assert!((fam.f0_hz - 2_336_398.45).abs() < 1.0, "f0 {}", fam.f0_hz);
    assert!(
        (fam.intercept_hz - 70.8).abs() < 1.0,
        "intercept {}",
        fam.intercept_hz
    );
    // The residual, against the tolerance the members' own widths set.
    assert!(
        fam.residual_rms_hz < fam.residual_tolerance_hz,
        "residual {} vs tolerance {}",
        fam.residual_rms_hz,
        fam.residual_tolerance_hz
    );
    assert!(
        (fam.residual_rms_hz - 190.7).abs() < 1.0,
        "residual {}",
        fam.residual_rms_hz
    );
    // The indices are pinned: the intercept's standard error is a ten-thousandth of a fundamental
    // away from the n <-> n+-1 coin flip at 0.5.
    assert!(
        fam.index_pin < 0.01,
        "index_pin {} must be far below {INDEX_PIN_MAX}",
        fam.index_pin
    );
    assert!(fam.origin_sigmas < 0.1, "origin {}", fam.origin_sigmas);
    // The width corroboration: rms width / n is 138 / 120 / 137 Hz.
    let s: Vec<f64> = fam.members.iter().map(|m| m.sigma0_hz).collect();
    assert!((s[0] - 138.0).abs() < 0.5 && (s[1] - 120.0).abs() < 0.5 && (s[2] - 137.0).abs() < 0.5);
    assert!(
        (fam.width.ratio - 1.150).abs() < 0.005,
        "width ratio {}",
        fam.width.ratio
    );
    // ...and the honest half of it: n spans 43..45, so `w ∝ n` and `w = const` differ by 4.7 %,
    // which 15 % of measured scatter cannot resolve. The evidence confirms the SCALE (a 28 kHz
    // emission is 43 x 134 Hz of one oscillator's frequency noise) and not the SLOPE, and it says
    // so rather than counting one reading twice.
    assert!(
        (fam.width.index_leverage - 45.0 / 43.0).abs() < 1e-9,
        "leverage {}",
        fam.width.index_leverage
    );
    assert!(
        !fam.width.separates,
        "at 1.047x of index leverage the width cannot separate w ∝ n from w = const"
    );
    eprintln!("{}", fam.arithmetic());
}

// -------------------------------------------------------------------------------------------
// 2. The independence demonstration
// -------------------------------------------------------------------------------------------

#[test]
fn off_by_one_leaves_every_residual_identical_and_moves_the_intercept_by_one_fundamental() {
    // This is the proof that the residual and the intercept are NOT two readings of one thing,
    // and equally that the residual can never pin an index. Relabelling every member n -> n+1
    // leaves `n - n̄` unchanged, so the least-squares slope and every residual are bit-identical,
    // while the intercept moves by exactly one f₀. Only the physics — a harmonic family passes
    // through the origin — can choose between the two labellings.
    let fam = judge_family(&t317(), &[43, 44, 45]).expect("T-317 is a family");
    for d in [-2, -1, 1, 2] {
        let shifted = fam.off_by_one(d).expect("in range");
        assert_eq!(
            shifted.f0_hz, fam.f0_hz,
            "the slope is invariant under a uniform index shift"
        );
        let base: Vec<f64> = fam.members.iter().map(|m| m.residual_hz).collect();
        assert_eq!(
            shifted.residuals, base,
            "every residual is bit-identical under a uniform index shift of {d}"
        );
        assert_eq!(
            shifted.residual_rms_hz, fam.residual_rms_hz,
            "so the residual cannot choose a labelling"
        );
        let moved = fam.intercept_hz - shifted.intercept_hz;
        assert!(
            (moved - f64::from(d) * fam.f0_hz).abs() < 1e-3,
            "the intercept moves by exactly {d} x f0: {moved} vs {}",
            f64::from(d) * fam.f0_hz
        );
        // And the shifted labelling is refused, because it misses the origin by a whole
        // fundamental at a precision of ten kilohertz.
        let shifted_idx: Vec<u32> = fam
            .members
            .iter()
            .map(|m| (i64::from(m.index) + i64::from(d)) as u32)
            .collect();
        assert_eq!(
            judge_family(&t317(), &shifted_idx),
            Err(FamilyRejection::NotThroughOrigin),
            "labelling shifted by {d}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// 3. The negative controls
// -------------------------------------------------------------------------------------------

#[test]
fn unrelated_real_emitters_are_not_declared_a_family() {
    let pool = unrelated_real_emitters();
    let found = find_harmonic_families(&pool);
    assert!(
        found.is_empty(),
        "eleven real unrelated emitters were declared {} famil(y/ies): {:#?}",
        found.len(),
        found.iter().map(|f| f.arithmetic()).collect::<Vec<_>>()
    );
}

/// A deterministic 64-bit LCG. No `rand` dependency in `hk-model`, and a fixed seed so the
/// measured false-family rate below is reproducible.
struct Lcg(u64);

impl Lcg {
    fn next_f64(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }
}

/// Draw `k` emitters uniformly over `[lo, hi]` with widths log-uniform over `[w_lo, w_hi]`,
/// rejecting draws whose bands overlap (two real emissions do not).
fn draw(rng: &mut Lcg, k: usize, lo: f64, hi: f64, w_lo: f64, w_hi: f64) -> Vec<FamilyMember> {
    let mut out: Vec<FamilyMember> = Vec::with_capacity(k);
    let mut guard = 0;
    while out.len() < k && guard < 10_000 {
        guard += 1;
        let f = rng.range(lo, hi);
        let w = (w_lo.ln() + (w_hi.ln() - w_lo.ln()) * rng.next_f64()).exp();
        if out
            .iter()
            .any(|m| (m.f_center_hz - f).abs() <= 0.5 * (m.width_hz + w))
        {
            continue;
        }
        out.push(member(out.len() as u8, f, w));
    }
    out
}

/// **The measured false-family rate.** A fit over enough emitters will always find some `f₀`, so
/// the only honest way to know what this mechanism claims is to run it on populations that are
/// related by nothing and count.
#[test]
fn random_populations_are_almost_never_declared_families() {
    // Four regimes: the two widths and the two spans that bracket what the front end sees.
    // `hi/MAX_INDEX` sets the smallest reachable fundamental, and the narrow-emitter regimes are
    // the hard ones, because their tolerance is the tightest but their count of reachable
    // labellings is the same.
    let regimes: [(&str, f64, f64, f64, f64); 4] = [
        ("broadcast FM band, 180 kHz wide", 88e6, 108e6, 150e3, 200e3),
        ("VHF, 8-200 kHz wide", 30e6, 300e6, 8e3, 200e3),
        ("UHF, 5-50 kHz wide", 300e6, 900e6, 5e3, 50e3),
        ("narrow lines, 2-20 kHz wide", 88e6, 108e6, 2e3, 20e3),
    ];
    let trials = 1000;
    let mut worst = 0.0f64;
    for (name, lo, hi, w_lo, w_hi) in regimes {
        let mut rng = Lcg(0x5eed_1234_abcd_0001);
        let mut hits = 0usize;
        let mut example = String::new();
        for _ in 0..trials {
            let pool = draw(&mut rng, 8, lo, hi, w_lo, w_hi);
            let found = find_harmonic_families(&pool);
            if let Some(f) = found.first() {
                hits += 1;
                if example.is_empty() {
                    example = f.arithmetic();
                }
            }
        }
        let rate = hits as f64 / trials as f64;
        eprintln!("[negative control] {name}: {hits}/{trials} = {rate:.4}  {example}");
        worst = worst.max(rate);
    }
    // The bound, not the observed number: this asserts the mechanism is a filter rather than a
    // rubber stamp. The measured rates are printed above and recorded in the report.
    assert!(
        worst <= 0.02,
        "unrelated emitters were declared families at {worst:.4}, which is not a filter"
    );
}

#[test]
fn a_bigger_population_does_not_buy_a_family() {
    // The other half of the vacuity worry: more rows to choose from is more chances to fit. Draw
    // wider populations and check the rate does not run away.
    let mut rng = Lcg(0xfeed_0000_0000_0007);
    let trials = 400;
    for k in [12usize, 20] {
        let mut hits = 0usize;
        for _ in 0..trials {
            let pool = draw(&mut rng, k, 30e6, 300e6, 8e3, 200e3);
            if !find_harmonic_families(&pool).is_empty() {
                hits += 1;
            }
        }
        let rate = hits as f64 / trials as f64;
        eprintln!("[negative control] {k} unrelated emitters: {hits}/{trials} = {rate:.4}");
        assert!(rate <= 0.05, "{k} emitters: {rate:.4}");
    }
}

// -------------------------------------------------------------------------------------------
// 4. Every gate says no, with a case that only it rejects
// -------------------------------------------------------------------------------------------

#[test]
fn a_family_may_not_cross_receive_chains() {
    // T-302/T-259: a harmonic family is a property of one front end's oscillator and one mixer.
    // Device B's antenna never had device A's oscillator in it, whatever the arithmetic says.
    let mut m = t317();
    m[2].chains = vec![ReceiveChain::device("hackrf-1")];
    assert_eq!(
        judge_family(&m, &[43, 44, 45]),
        Err(FamilyRejection::ChainsDiffer)
    );
    assert!(find_harmonic_families(&m).is_empty());
    // The identical geometry on one chain still attributes (no regression on the positive case).
    assert!(judge_family(&t317(), &[43, 44, 45]).is_ok());
}

#[test]
fn a_family_may_not_cross_antenna_ports() {
    let mut m = t317();
    for (i, x) in m.iter_mut().enumerate() {
        x.chains = vec![ReceiveChain {
            device_id: "hackrf-0".into(),
            antenna_port: Some(if i == 2 { "B0" } else { "A1" }.into()),
        }];
    }
    assert_eq!(
        judge_family(&m, &[43, 44, 45]),
        Err(FamilyRejection::ChainsDiffer)
    );
}

#[test]
fn widths_that_contradict_the_harmonic_prediction_reject() {
    // The one genuinely independent column. Member 44 is made 600 Hz/n wide against its
    // siblings' 138; the frequency fit is untouched and still perfect.
    let mut m = t317();
    m[1].width_hz = 44.0 * 600.0;
    assert_eq!(
        judge_family(&m, &[43, 44, 45]),
        Err(FamilyRejection::WidthsInconsistent)
    );
    // ...and the frequency evidence really is unchanged, so this rejection is the widths talking.
    let freqs: Vec<f64> = m.iter().map(|x| x.f_center_hz).collect();
    let a = fit_line(&[43, 44, 45], &freqs).unwrap();
    let b = fit_line(
        &[43, 44, 45],
        &t317().iter().map(|x| x.f_center_hz).collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(a.f0_hz, b.f0_hz);
    assert_eq!(a.intercept_hz, b.intercept_hz);
}

#[test]
fn width_evidence_separates_the_two_models_when_the_indices_give_it_lever() {
    // At T-317's 45/43 it cannot (asserted above). Over n = 3..12 of one oscillator it can, and
    // the flat rival is measurably worse.
    let n: Vec<u32> = (3..=12).collect();
    let sigma0 = 140.0;
    let widths: Vec<f64> = n.iter().map(|&k| sigma0 * f64::from(k)).collect();
    let w = width_evidence(&n, &widths);
    assert!(w.separates, "{w:?}");
    assert!(w.rss_harmonic < 1e-6 * w.rss_flat, "{w:?}");
    // Constant-width emissions over the same indices are NOT a harmonic family's widths.
    let flat: Vec<f64> = n.iter().map(|_| 1_000.0).collect();
    let wf = width_evidence(&n, &flat);
    assert!(!wf.separates, "{wf:?}");
    assert!(wf.ratio > WIDTH_RATIO_MAX, "ratio {}", wf.ratio);
}

#[test]
fn an_arithmetic_comb_offset_from_dc_is_not_a_harmonic_family() {
    // Perfectly collinear, perfectly pinned, zero residual — and refused, because the grid misses
    // the origin by half a megahertz. This is the distinction the whole mechanism turns on: a
    // comb of spacing f₀ offset from DC is a mixing product grid, not harmonics of a fundamental.
    let m = vec![
        member(1, 100_500_000.0, 5_000.0),
        member(2, 102_500_000.0, 5_000.0),
        member(3, 104_500_000.0, 5_000.0),
    ];
    let r = judge_family(&m, &[50, 51, 52]);
    assert_eq!(r, Err(FamilyRejection::NotThroughOrigin));
    // The search cannot rescue it with a different labelling either.
    assert!(find_harmonic_families(&m).is_empty());
}

#[test]
fn indices_that_are_not_pinned_are_refused_and_that_is_the_better_refusal() {
    // **The non-vacuity gate.** Three 120 kHz-wide emissions near harmonics 49..51 of a 2 MHz
    // oscillator. A 120 kHz box cannot pin harmonic 50: extrapolate its centre precision down to
    // zero frequency and the intercept carries a quarter of a fundamental, so n and n +- 1 are the
    // same hypothesis and there is no labelling to claim. That is checked BEFORE the residual
    // because it is the more informative answer — "no determinate grid exists here" rather than
    // "you are some kilohertz off one particular grid".
    let m = vec![
        member(1, 98_008_000.0, 120_000.0),
        member(2, 99_992_000.0, 120_000.0),
        member(3, 102_008_000.0, 120_000.0),
    ];
    let idx = [49u32, 50, 51];
    let freqs: Vec<f64> = m.iter().map(|x| x.f_center_hz).collect();
    let fit = fit_line(&idx, &freqs).unwrap();
    assert!(
        fit.intercept_hz.abs() / fit.intercept_se_hz < ORIGIN_MAX_SIGMA,
        "the self-scaling origin test would have passed it at {} sigma",
        fit.intercept_hz.abs() / fit.intercept_se_hz
    );
    let pin = fit.intercept_se_hz / fit.f0_hz;
    assert!(
        pin > 0.2,
        "the indices are not pinned: se(b) is {pin:.3} of a fundamental"
    );
    assert_eq!(
        judge_family(&m, &idx),
        Err(FamilyRejection::IndicesNotPinned)
    );
}

#[test]
fn within_tolerance_the_indices_are_pinned_by_construction() {
    // **The non-vacuity condition, enumerated rather than hoped for.** `INDEX_PIN_MAX` is a guard,
    // and the shipped constants make it one that cannot fire on a set which clears the residual
    // gate: RESIDUAL_PPM and MAX_INDEX together bound `se(b)/f0`. Exhaustive over every 3-member
    // index set in MIN_INDEX..=MAX_INDEX, which is the worst case — both `sqrt(k/(k-2))` and
    // `1/Snn` fall as members are added, so no larger k can beat it.
    //
    // The bound is what makes the mechanism mean something: it says that inside the tolerance this
    // module admits, the integer labelling of a family is DETERMINED, not chosen. Widen
    // RESIDUAL_PPM or raise MAX_INDEX and this test is where it shows up.
    let mut worst = 0.0f64;
    let mut at = (0u32, 0u32, 0u32);
    for a in MIN_INDEX..=MAX_INDEX {
        for b in a + 1..=MAX_INDEX {
            for c in b + 1..=MAX_INDEX {
                let n = [f64::from(a), f64::from(b), f64::from(c)];
                let n_bar = (n[0] + n[1] + n[2]) / 3.0;
                let snn: f64 = n.iter().map(|x| (x - n_bar) * (x - n_bar)).sum();
                // sigma_hat <= residual_rms * sqrt(k/(k-2)), residual_rms <= RESIDUAL_PPM * f_max,
                // and f0 is at most f_max / c (a lower index means a larger f0, so a smaller pin).
                let pin = RESIDUAL_PPM
                    * 1e-6
                    * (3.0f64 / 1.0).sqrt()
                    * (1.0 / 3.0 + n_bar * n_bar / snn).sqrt()
                    * f64::from(c);
                if pin > worst {
                    worst = pin;
                    at = (a, b, c);
                }
            }
        }
    }
    eprintln!("[non-vacuity] worst reachable index_pin = {worst:.4} at n = {at:?}");
    assert!((worst - 0.0247).abs() < 0.001, "worst {worst}");
    assert!(
        worst < INDEX_PIN_MAX,
        "every residual-passing set is pinned by construction"
    );
}

#[test]
fn lines_inside_one_emission_are_a_comb_and_not_a_family() {
    // `hk_detect::comb` fits n x f0 over spectral lines WITHIN one detection. That is a different
    // mechanism with a different tolerance, and this one refuses to impersonate it.
    let m = vec![
        member(1, 100_000_000.0, 5_000.0),
        member(2, 100_002_000.0, 5_000.0),
        member(3, 100_004_000.0, 5_000.0),
    ];
    assert_eq!(
        judge_family(&m, &[50, 51, 52]),
        Err(FamilyRejection::OverlappingMembers)
    );
}

#[test]
fn line_shapes_are_all_or_nothing_and_can_only_reject() {
    let shape = |phase: f64| -> Vec<f64> {
        (0..32)
            .map(|i| {
                let x = f64::from(i) / 31.0 * 6.0 - 3.0;
                (-(x - phase) * (x - phase) / 2.0).exp()
            })
            .collect()
    };
    // Agreeing shapes: the family stands, and the correlation is reported.
    let mut m = t317();
    for x in &mut m {
        x.line_shape = Some(shape(0.0));
    }
    let fam = judge_family(&m, &[43, 44, 45]).expect("shapes agree");
    assert!(fam.line_shape_corr.is_some_and(|c| c > 0.99));
    // A shape that disagrees rejects a family the frequency and width evidence both accepted.
    let mut m = t317();
    m[0].line_shape = Some(shape(0.0));
    m[1].line_shape = Some(shape(0.0));
    m[2].line_shape = Some(shape(2.5));
    assert_eq!(
        judge_family(&m, &[43, 44, 45]),
        Err(FamilyRejection::LineShapesDisagree)
    );
    // A shape only some members took is not a test.
    let mut m = t317();
    m[0].line_shape = Some(shape(0.0));
    assert_eq!(
        judge_family(&m, &[43, 44, 45]),
        Err(FamilyRejection::LineShapesDisagree)
    );
}

#[test]
fn every_rejection_has_been_observed() {
    // **The non-vacuity demonstration for the "no" side.** A mechanism whose "no" you have never
    // observed is not known to have one, so this collects an actual rejection of every kind the
    // type can express and fails if any is unreachable.
    use FamilyRejection::*;
    let shape: Vec<f64> = (0..32).map(f64::from).collect();

    let mut chain_b = t317();
    chain_b[2].chains = vec![ReceiveChain::device("hackrf-1")];
    let mut wide = t317();
    wide[1].width_hz = 44.0 * 600.0;
    let mut half_shape = t317();
    half_shape[0].line_shape = Some(shape);
    let mut unmeasured = t317();
    unmeasured[1].width_hz = 0.0;

    let cases: Vec<(FamilyRejection, Vec<FamilyMember>, Vec<u32>)> = vec![
        (TooFewMembers, t317()[..2].to_vec(), vec![43, 44]),
        (Unmeasured, unmeasured, vec![43, 44, 45]),
        (IndexOutOfRange, t317(), vec![43, 44, 65]),
        (DuplicateIndex, t317(), vec![43, 44, 44]),
        (
            OverlappingMembers,
            vec![
                member(1, 100_000_000.0, 5_000.0),
                member(2, 100_002_000.0, 5_000.0),
                member(3, 100_004_000.0, 5_000.0),
            ],
            vec![50, 51, 52],
        ),
        (ChainsDiffer, chain_b, vec![43, 44, 45]),
        // Pinned to 7.1e-4 of a fundamental — the labelling is not in doubt — and still 283 Hz
        // rms off the grid against a 50 Hz tolerance.
        (
            ResidualTooLarge,
            vec![
                member(1, 6_000_200.0, 5_000.0),
                member(2, 7_999_600.0, 5_000.0),
                member(3, 10_000_200.0, 5_000.0),
            ],
            vec![3, 4, 5],
        ),
        (
            IndicesNotPinned,
            vec![
                member(1, 98_008_000.0, 120_000.0),
                member(2, 99_992_000.0, 120_000.0),
                member(3, 102_008_000.0, 120_000.0),
            ],
            vec![49, 50, 51],
        ),
        (
            NotThroughOrigin,
            vec![
                member(1, 100_500_000.0, 5_000.0),
                member(2, 102_500_000.0, 5_000.0),
                member(3, 104_500_000.0, 5_000.0),
            ],
            vec![50, 51, 52],
        ),
        (WidthsInconsistent, wide, vec![43, 44, 45]),
        (LineShapesDisagree, half_shape, vec![43, 44, 45]),
    ];
    let mut seen: Vec<FamilyRejection> = Vec::new();
    for (want, members, idx) in cases {
        let got = judge_family(&members, &idx);
        assert_eq!(got, Err(want), "expected {want:?}, got {got:?}");
        assert!(!want.reason().is_empty());
        seen.push(want);
    }
    // Every variant the type can express is in the list above. Adding one without a case that
    // reaches it fails here.
    for want in [
        TooFewMembers,
        Unmeasured,
        DuplicateIndex,
        OverlappingMembers,
        ChainsDiffer,
        IndexOutOfRange,
        ResidualTooLarge,
        IndicesNotPinned,
        NotThroughOrigin,
        WidthsInconsistent,
        LineShapesDisagree,
    ] {
        assert!(seen.contains(&want), "no case reaches {want:?}");
    }
}

#[test]
fn a_visible_fundamental_is_relates_case_not_this_one() {
    // n = 1 is the fundamental itself, and `relate::predict_artifacts` already attributes n·f to a
    // confirmed source row. This mechanism is for the fundamental nobody can see.
    assert_eq!(
        judge_family(&t317(), &[1, 44, 45]),
        Err(FamilyRejection::IndexOutOfRange)
    );
}

#[test]
fn two_points_are_never_a_family_however_they_are_labelled() {
    // Two points and two free parameters fit exactly: the residual is identically zero and
    // nothing has been tested.
    let m = t317();
    assert_eq!(
        judge_family(&m[..2], &[43, 44]),
        Err(FamilyRejection::TooFewMembers)
    );
    assert!(fit_line(&[43, 44], &[100e6, 102e6]).is_none());
}
