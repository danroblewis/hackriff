//! T-164 (ADR-0013 §4.9 gap 7b, ADR-0011 §2.4): ranking recipes against a signal's **measured**
//! parameters.
//!
//! These tests pin the three rules the route exists to enforce, over the recipes actually shipped
//! in `recipes/` wherever possible, so a change to a built-in recipe's `match` block is caught
//! here rather than in an e2e run:
//!
//! 1. measurement decides the order, and a band-plan hint may only break an exact tie;
//! 2. an unmeasured parameter is never agreement;
//! 3. an emission that fits nothing gets an empty ranking, not a forced top choice.

use std::path::PathBuf;

use hk_recipe::matching::{
    LOW_CONFIDENCE, MeasuredSignal, NO_CANDIDATE, Outcome, RULED_OUT_FAMILY,
    RULED_OUT_TOO_FEW_COMPARED, Ranked, TOO_FEW_MEASUREMENTS, Verdict, rank,
};
use hk_recipe::{Entry, MatchHints, Recipe};

/// The recipes the repository ships, as ranking entries.
fn builtins() -> Vec<Entry> {
    ["rds", "acars", "adsb", "pocsag"]
        .iter()
        .map(builtin)
        .collect()
}

fn builtin(id: &&str) -> Entry {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../recipes")
        .join(format!("{id}.recipe.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let doc: Recipe = serde_json::from_str(&text).unwrap();
    Entry {
        id: doc.id.clone(),
        version: doc.version,
        name: doc.name.clone(),
        hints: doc.match_hints,
    }
}

fn ruled(r: &Ranked, id: &str) -> String {
    r.ruled_out
        .iter()
        .find(|x| x.id == id)
        .unwrap_or_else(|| panic!("{id} is not in ruled_out: {:?}", r.ruled_out))
        .reason
        .clone()
}

fn reason<'a>(c: &'a hk_recipe::Candidate, field: &str) -> &'a hk_recipe::MatchReason {
    c.reasons
        .iter()
        .find(|r| r.field == field)
        .unwrap_or_else(|| panic!("no reason for {field}: {:?}", c.reasons))
}

/// What a WFM broadcast station measures like: the family the classifier called, the bandwidth
/// detection measured, a pilot the demodulator locked, and a duty cycle saying it is continuous.
/// Nothing here is looked up; the centre is carried only so the tie-break has something to read.
fn measured_fm_station(f_center_hz: f64) -> MeasuredSignal {
    MeasuredSignal {
        family: Some("wfm".to_owned()),
        f_center_hz: Some(f_center_hz),
        bandwidth_hz: Some(180e3),
        symbol_rate_bd: None,
        bursty: Some(false),
        features: vec!["pilot-19k".to_owned()],
    }
}

/// The blind result: an FM broadcast station ranks the RDS recipe top **because of what was
/// measured on it**, with the reasons cited, and the other three shipped recipes are ruled out on
/// the measured modulation family rather than merely scoring lower.
#[test]
fn the_fm_station_ranks_the_rds_recipe_first_from_measurement_alone() {
    let r = rank(&builtins(), &measured_fm_station(101.3e6));

    assert_eq!(r.candidates.len(), 1, "{:#?}", r.candidates);
    let best = &r.candidates[0];
    assert_eq!(best.id, "rds");
    assert_eq!(best.outcome, Outcome::Fit);
    assert!(best.score > 0.99, "score {}", best.score);
    assert_eq!(best.conflicting, 0);

    // The reasons say *why*, field by field.
    assert_eq!(reason(best, "family").verdict, Verdict::Agree);
    assert_eq!(reason(best, "bandwidth_hz").verdict, Verdict::Agree);
    assert_eq!(reason(best, "bursty").verdict, Verdict::Agree);
    assert_eq!(reason(best, "feature:pilot-19k").verdict, Verdict::Agree);
    assert!(
        reason(best, "feature:pilot-19k")
            .detail
            .contains("was measured"),
        "{:?}",
        reason(best, "feature:pilot-19k").detail
    );

    // A different modulation is ruled out, not ranked low.
    for id in ["acars", "adsb", "pocsag"] {
        assert_eq!(ruled(&r, id), RULED_OUT_FAMILY, "{id}");
    }
}

/// The blind rule, stated as sharply as it can be: move the station to a frequency **outside**
/// every range the RDS recipe declares, change nothing else, and it still ranks first. The
/// ranking is reading the measurement, not the band plan.
#[test]
fn the_ranking_does_not_depend_on_where_the_signal_was_found() {
    let in_band = rank(&builtins(), &measured_fm_station(101.3e6));
    let out_of_band = rank(&builtins(), &measured_fm_station(451.0e6));

    assert_eq!(out_of_band.candidates[0].id, "rds");
    assert_eq!(
        in_band.candidates[0].score, out_of_band.candidates[0].score,
        "the score must not move with the frequency"
    );
    // Only the tie-break flag notices the move.
    assert!(in_band.candidates[0].band_hint);
    assert!(!out_of_band.candidates[0].band_hint);
}

fn hints(bw: [f64; 2], rate: [f64; 2], freq: Vec<[f64; 2]>) -> MatchHints {
    MatchHints {
        families: vec!["fsk".to_owned()],
        freq_hz: freq,
        bandwidth_hz: Some(bw),
        symbol_rate_bd: Some(rate),
        bursty: Some(true),
        features: vec![],
    }
}

fn entry(id: &str, hints: MatchHints) -> Entry {
    Entry {
        id: id.to_owned(),
        version: 1,
        name: id.to_uppercase(),
        hints,
    }
}

/// A measured FSK burst: 12.5 kHz wide, 1200 Bd, bursty, at 446.1 MHz.
fn measured_fsk() -> MeasuredSignal {
    MeasuredSignal {
        family: Some("fsk2".to_owned()),
        f_center_hz: Some(446.1e6),
        bandwidth_hz: Some(12.5e3),
        symbol_rate_bd: Some(1200.0),
        bursty: Some(true),
        features: vec![],
    }
}

/// **The rule this route lives by** (T-212, applied to recipes): the measurements favour `alpha`,
/// while the band plan puts the signal squarely inside `bravo`'s declared range. `alpha` ranks
/// first anyway. A band-plan hint may not promote a worse-matching recipe.
#[test]
fn a_band_plan_hint_never_outranks_a_better_measurement() {
    let alpha = entry("alpha", hints([10e3, 15e3], [1000.0, 1500.0], vec![]));
    // Same shape, but its declared bandwidth and rate are both well off the measurement — and it
    // is the one the band plan points at.
    let bravo = entry(
        "bravo",
        hints([20e3, 30e3], [2000.0, 3000.0], vec![[446.0e6, 446.2e6]]),
    );

    let r = rank(&[bravo, alpha], &measured_fsk());

    assert_eq!(r.candidates[0].id, "alpha", "{:#?}", r.candidates);
    assert!(r.candidates[0].score > r.candidates[1].score);
    // The hint was read, and it still lost to the measurement.
    assert!(!r.candidates[0].band_hint);
    assert!(r.candidates[1].band_hint, "bravo is the band plan's pick");
}

/// The other half of the same rule: where the measurements genuinely cannot separate two
/// recipes, the band-plan hint is allowed to break the tie — and only then. `alpha` sorts first
/// alphabetically, so a hint that did nothing would leave it on top.
#[test]
fn a_band_plan_hint_breaks_an_exact_tie() {
    let alpha = entry("alpha", hints([10e3, 15e3], [1000.0, 1500.0], vec![]));
    let bravo = entry(
        "bravo",
        hints([10e3, 15e3], [1000.0, 1500.0], vec![[446.0e6, 446.2e6]]),
    );

    let r = rank(&[alpha, bravo], &measured_fsk());

    assert_eq!(r.candidates.len(), 2);
    assert_eq!(
        r.candidates[0].score, r.candidates[1].score,
        "the measurements do not separate them"
    );
    assert_eq!(r.candidates[0].id, "bravo", "the hint breaks the tie");
    assert!(r.candidates[0].band_hint);
}

/// T-163 serves `null` for a parameter nothing measured rather than a default, so that absence
/// can never be mistaken for evidence. Ranking must honour that: dropping a measurement lowers
/// the score instead of leaving it alone.
#[test]
fn an_unmeasured_parameter_never_counts_as_agreement() {
    let e = vec![entry(
        "alpha",
        hints([10e3, 15e3], [1000.0, 1500.0], vec![]),
    )];

    let all_measured = rank(&e, &measured_fsk());
    let mut without_rate = measured_fsk();
    without_rate.symbol_rate_bd = None;
    let partial = rank(&e, &without_rate);

    let full = &all_measured.candidates[0];
    let thin = &partial.candidates[0];
    assert!(full.score > 0.99, "score {}", full.score);
    assert!(
        thin.score < full.score,
        "an unmeasured symbol rate must cost score: {} vs {}",
        thin.score,
        full.score
    );

    let r = reason(thin, "symbol_rate_bd");
    assert_eq!(r.verdict, Verdict::Unmeasured);
    assert_eq!(r.earned, 0.0, "an unmeasured field earns nothing");
    assert!(r.measured.is_null());
    assert!(
        r.weight > 0.0,
        "and its weight still counts against the score"
    );
    assert_eq!(thin.unmeasured, 1);
}

/// An emission nothing has really characterised — a bare 3 kHz carrier, family unknown — gets an
/// **empty** ranking. It would be easy to hand back the recipe whose declared bandwidth happens
/// to contain 3 kHz; that is the forced, confidently-wrong answer this route must never give.
#[test]
fn an_emission_that_fits_nothing_is_not_given_a_match() {
    let measured = MeasuredSignal {
        family: None,
        f_center_hz: Some(145.8e6),
        bandwidth_hz: Some(3e3),
        symbol_rate_bd: None,
        bursty: None,
        features: vec![],
    };

    let r = rank(&builtins(), &measured);

    assert!(r.candidates.is_empty(), "{:#?}", r.candidates);
    assert_eq!(r.outcome, Outcome::None);
    assert!(
        r.reasons.iter().any(|x| x == NO_CANDIDATE),
        "{:?}",
        r.reasons
    );
    assert!(
        r.reasons.iter().any(|x| x == TOO_FEW_MEASUREMENTS),
        "{:?}",
        r.reasons
    );
    // ACARS declares 2–8 kHz, which contains this carrier's only measurement. It is still not
    // offered: one comparison out of four expectations is not a match.
    assert_eq!(ruled(&r, "acars"), RULED_OUT_TOO_FEW_COMPARED);
}

/// A measured family the recipe cannot decode rules it out outright, rather than leaving it in
/// the list with a low score where a client might still offer it.
#[test]
fn a_conflicting_family_is_ruled_out_rather_than_ranked_low() {
    let mut am = measured_fm_station(101.3e6);
    am.family = Some("am".to_owned());
    am.features.clear();

    let r = rank(&builtins(), &am);

    assert_eq!(ruled(&r, "rds"), RULED_OUT_FAMILY);
    assert!(
        !r.candidates.iter().any(|c| c.id == "rds"),
        "{:#?}",
        r.candidates
    );
}

/// Every offered candidate clears the confidence floor, on every measurement these tests use.
#[test]
fn nothing_below_the_confidence_floor_is_ever_offered() {
    for m in [
        measured_fm_station(101.3e6),
        measured_fsk(),
        MeasuredSignal::default(),
    ] {
        for c in rank(&builtins(), &m).candidates {
            assert!(c.score >= LOW_CONFIDENCE, "{} scored {}", c.id, c.score);
        }
    }
}
