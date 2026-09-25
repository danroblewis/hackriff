//! T-856 (MAUTO M-5): the template library — loader, consistency validation, built-ins, seeding.
//! ADR-0015 §4.1, §4.2, §15.6. Blind rule: templates order the search, never rank or confirm.

use hk_model::classify::{BudgetHint, Hypothesis, SeedBoost};
use hk_model::signature::RecipeRef;
use hk_synth::candidate::SeedSource;
use hk_synth::library::{
    MISMATCH_FACTOR, MeasuredParams, TemplateErrorCode, TemplateLibrary, builtin_recipes,
};
use hk_synth::stage::Stage;

fn hyp(family: &str, posterior: f64, prune: bool) -> Hypothesis {
    Hypothesis {
        family: family.into(),
        posterior,
        likelihood: 0.5,
        prune,
        below_gate: false,
        boosts: vec![],
        recipes: vec![],
        missing: vec![],
        reasons: vec![],
    }
}

fn hint(hs: Vec<Hypothesis>, share: f64) -> BudgetHint {
    BudgetHint {
        open_search_min_share: share,
        p_unknown: share,
        families_ordered: hs,
    }
}

#[test]
fn builtins_load_one_per_recipe_plus_generic_skeletons_including_psk() {
    let lib = TemplateLibrary::builtin().expect("shipped templates validate");
    for id in ["rds", "pocsag", "acars", "adsb"] {
        assert!(
            lib.get(&format!("{id}@1"))
                .unwrap()
                .template
                .recipe
                .is_some(),
            "{id}"
        );
    }
    for id in [
        "generic-fsk-framed",
        "generic-ook-pwm",
        "generic-ook-manchester",
        "generic-msk",
        "generic-ppm",
        "generic-psk-framed",
    ] {
        assert!(
            lib.get(&format!("{id}@1"))
                .unwrap()
                .template
                .skeleton
                .is_some(),
            "{id}"
        );
    }
    // PSK reaches bits: its S1 alternatives commit to the psk family with psk_demod.
    let psk = lib
        .skeleton("generic-psk-framed@1")
        .expect("psk is not inert");
    assert!(psk.alternatives(Stage::S1).len() >= 4);
    assert!(
        psk.alternatives(Stage::S1)
            .iter()
            .all(|a| a.family.as_deref() == Some("psk"))
    );
}

#[test]
fn a_skeleton_naming_a_missing_block_is_inert_and_never_seeded() {
    let lib = TemplateLibrary::builtin().unwrap();
    let inert: Vec<_> = lib.inert().collect();
    assert!(
        inert
            .iter()
            .any(|(k, b)| *k == "generic-ook-pwm@1" && b.contains(&"pwm_decode".to_string()))
    );
    assert!(lib.skeleton("generic-ook-pwm@1").is_none());
    let s = lib.seed(
        &hint(vec![hyp("ook", 0.6, false)], 0.2),
        &MeasuredParams::default(),
    );
    assert!(s.ordered.iter().all(|t| t.template != "generic-ook-pwm@1"));
    assert!(
        s.ordered
            .iter()
            .any(|t| t.template == "generic-ook-manchester@1")
    );
}

fn mutate(
    lib_key: &str,
    f: impl FnOnce(&mut serde_json::Value),
) -> Result<(), hk_synth::library::TemplateError> {
    let text = match lib_key {
        "pocsag" => include_str!("../../../templates/pocsag.template.json"),
        _ => include_str!("../../../templates/generic-msk.template.json"),
    };
    let mut v: serde_json::Value = serde_json::from_str(text).unwrap();
    f(&mut v);
    let recipes = builtin_recipes().unwrap();
    let reg = hk_blocks::Registry::builtin();
    TemplateLibrary::default().add_text("t", &v.to_string(), &recipes, &reg)
}

#[test]
fn consistency_refuses_incoherent_templates() {
    let code = |r: Result<(), hk_synth::library::TemplateError>| r.unwrap_err().code;
    assert_eq!(
        code(mutate(
            "pocsag",
            |v| v["evidence_targets"]["S4"]["sync_bits"] = 24.into()
        )),
        TemplateErrorCode::SyncBits
    );
    assert_eq!(
        code(mutate("pocsag", |v| v["free"][0]["path"] =
            "nodes[nope].params.x".into())),
        TemplateErrorCode::FreePath
    );
    assert_eq!(
        code(mutate("pocsag", |v| v["recipe"]["version"] = 99.into())),
        TemplateErrorCode::RecipeMissing
    );
    assert_eq!(
        code(mutate("pocsag", |v| v["priors"]["bandwidth_hz"] =
            serde_json::json!([9e3, 1e3]))),
        TemplateErrorCode::Range
    );
    assert_eq!(
        code(mutate("pocsag", |v| v["provenance"]["facts"] =
            serde_json::json!([]))),
        TemplateErrorCode::FactUnsourced
    );
    assert_eq!(
        code(mutate("msk", |v| {
            v["recipe"] = serde_json::json!({"id":"pocsag","version":1});
        })),
        TemplateErrorCode::Structure
    );
    assert_eq!(
        code(mutate("msk", |v| v["schema_version"] = 2.into())),
        TemplateErrorCode::Schema
    );
    assert_eq!(
        code(mutate("msk", |v| v["tune_to_hz"] = 1.0.into())),
        TemplateErrorCode::Parse
    );
    // The same file loads unmutated.
    assert!(mutate("pocsag", |_| {}).is_ok());
}

#[test]
fn versions_are_immutable_and_user_dirs_load() {
    let mut lib = TemplateLibrary::builtin().unwrap();
    let dup = include_str!("../../../templates/generic-msk.template.json");
    let e = lib
        .add_text(
            "dup",
            dup,
            &builtin_recipes().unwrap(),
            &hk_blocks::Registry::builtin(),
        )
        .unwrap_err();
    assert_eq!(e.code, TemplateErrorCode::Duplicate);
    let dir = std::env::temp_dir().join(format!("hk-synth-tpl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(dup).unwrap();
    v["id"] = "my-msk".into();
    v["provenance"] = serde_json::json!({"kind": "user"});
    std::fs::write(dir.join("my-msk.json"), v.to_string()).unwrap();
    let n = lib
        .load_dir(
            &dir,
            &builtin_recipes().unwrap(),
            &hk_blocks::Registry::builtin(),
        )
        .unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(n, 1);
    assert!(lib.get("my-msk@1").is_some());
    assert_eq!(
        TemplateLibrary::default()
            .load_dir(&dir, &[], &hk_blocks::Registry::builtin())
            .unwrap(),
        0
    );
}

#[test]
fn seeding_orders_by_prior_and_a_mismatch_demotes_but_never_drops() {
    let lib = TemplateLibrary::builtin().unwrap();
    let h = hint(
        vec![
            hyp("fsk", 0.5, false),
            hyp("psk", 0.3, false),
            hyp("ook", 0.1, true),
        ],
        0.25,
    );
    let fits = MeasuredParams {
        symbol_rate_bd: Some(1200.0),
        bursty: Some(true),
        ..Default::default()
    };
    let s = lib.seed(&h, &fits);
    let bits = |s: &hk_synth::library::TemplateSeeding, k: &str| {
        s.ordered
            .iter()
            .find(|t| t.template == k)
            .map(|t| t.prior_bits)
    };
    // psk is reachable from classification alone.
    assert!(bits(&s, "generic-psk-framed@1").is_some());
    // pocsag (fsk) beats the generic fsk skeleton only through match: both present, none dropped.
    assert!(bits(&s, "pocsag@1").is_some() && bits(&s, "generic-fsk-framed@1").is_some());
    // Deferred families come last.
    let first_deferred = s.ordered.iter().position(|t| t.deferred).unwrap();
    assert!(s.ordered[first_deferred..].iter().all(|t| t.deferred));
    // Contradicting the measured symbol rate lowers pocsag's prior by exactly MISMATCH_FACTOR.
    let off = MeasuredParams {
        symbol_rate_bd: Some(9600.0),
        bursty: Some(true),
        ..Default::default()
    };
    let s2 = lib.seed(&h, &off);
    let d = bits(&s, "pocsag@1").unwrap() - bits(&s2, "pocsag@1").unwrap();
    assert!((f64::from(d) - -MISMATCH_FACTOR.log2()).abs() < 1e-3, "{d}");
    // No template order ever depends on a tuned frequency: absent centre → same order as none.
    assert_eq!(s.open_search_min_share, 0.25);
    assert!(
        s.open_skeletons
            .contains(&"generic-psk-framed@1".to_string())
    );
}

#[test]
fn bands_rank_only_for_an_already_detected_emitter() {
    let lib = TemplateLibrary::builtin().unwrap();
    let h = hint(vec![hyp("wfm", 0.5, false)], 0.2);
    let base = lib.seed(&h, &MeasuredParams::default());
    let inband = lib.seed(
        &h,
        &MeasuredParams {
            centre_hz: Some(100e6),
            ..Default::default()
        },
    );
    let get = |s: &hk_synth::library::TemplateSeeding| {
        s.ordered
            .iter()
            .find(|t| t.template == "rds@1")
            .unwrap()
            .prior_bits
    };
    assert!((f64::from(get(&inband) - get(&base)) - 1.5f64.log2()).abs() < 1e-3);
    // Nothing is created from a band: without a detection the template is still just ordered.
    assert!(
        base.ordered
            .iter()
            .all(|t| t.seed_source == SeedSource::Classification)
    );
}

#[test]
fn a_full_signature_match_takes_the_fast_path_first() {
    let lib = TemplateLibrary::builtin().unwrap();
    let mut boosted = hyp("fsk", 0.05, false);
    boosted.boosts.push(SeedBoost::SignatureFull);
    boosted.recipes.push(RecipeRef {
        id: "pocsag".into(),
        version: 1,
    });
    let h = hint(vec![hyp("psk", 0.9, false), boosted], 0.2);
    let s = lib.seed(&h, &MeasuredParams::default());
    assert_eq!(s.ordered[0].template, "pocsag@1");
    assert!(s.ordered[0].fast_path);
    assert_eq!(s.ordered[0].seed_source, SeedSource::Signature);
    assert!(!s.ordered[1].fast_path);
}

#[test]
fn open_search_share_floor_is_at_least_a_fifth() {
    let lib = TemplateLibrary::builtin().unwrap();
    let s = lib.seed(&hint(vec![], 0.05), &MeasuredParams::default());
    assert_eq!(s.open_search_min_share, 0.2);
    assert!(s.ordered.is_empty() && !s.open_skeletons.is_empty());
}

/// ADR-0022 §5.1, the laundering rule (T-575). A template a search **discovered** must carry the
/// look-elsewhere that search spent, and every later use inherits it as `L_check`; only `builtin`
/// or `user` templates that fix the whole check are template-fixed. Without this, a search could
/// try 10⁴ polynomials, save the winner, and confirm with it free forever.
#[test]
fn a_discovered_template_carries_and_passes_on_its_discovery_look_elsewhere() {
    use hk_synth::result::CheckOrigin;
    let recipes = builtin_recipes().unwrap();
    let reg = hk_blocks::Registry::builtin();
    let adsb: serde_json::Value =
        serde_json::from_str(include_str!("../../../templates/adsb.template.json")).unwrap();
    let load = |v: &serde_json::Value| {
        let mut lib = TemplateLibrary::default();
        lib.add_text("t", &v.to_string(), &recipes, &reg)
            .map(|()| lib)
    };

    // The shipped builtin: recipe-backed, nothing free — template-fixed, L_check 0.
    let lib = load(&adsb).unwrap();
    let t = &lib.get("adsb@1").unwrap().template;
    assert_eq!(t.check_origin(), CheckOrigin::TemplateFixed);

    // The same document saved from an analyze result: discovered, priced by its search.
    let mut found = adsb.clone();
    found["id"] = "adsb-found".into();
    found["provenance"] = serde_json::json!({
        "kind": "discovered", "job_id": "a7", "discovery_look_elsewhere_bits": 21.5 });
    let lib = load(&found).unwrap();
    let t = &lib.get("adsb-found@1").unwrap().template;
    assert_eq!(
        t.check_origin(),
        CheckOrigin::Discovered {
            look_elsewhere_bits: Some(21.5)
        }
    );
    assert!(
        t.check_origin().searched(),
        "inherited L ⇒ the null control gates it"
    );
    assert_eq!(t.check_origin().inherited_bits(), Some(21.5));

    // Seeding hands the origin to the search, so the root built from it pays.
    let seeded = lib.seed(
        &hint(vec![hyp("ppm", 0.9, false)], 0.2),
        &MeasuredParams::default(),
    );
    let s = seeded
        .ordered
        .iter()
        .find(|s| s.template == "adsb-found@1")
        .expect("seeded under ppm");
    assert_eq!(s.check_origin, t.check_origin());

    // Unpriced: refused at load — an unknown charge is not a zero one.
    let mut unpriced = found.clone();
    unpriced["provenance"]
        .as_object_mut()
        .unwrap()
        .remove("discovery_look_elsewhere_bits");
    assert_eq!(
        load(&unpriced).unwrap_err().code,
        TemplateErrorCode::DiscoveryUnpriced
    );
    let mut negative = found.clone();
    negative["provenance"]["discovery_look_elsewhere_bits"] = (-1.0).into();
    assert_eq!(
        load(&negative).unwrap_err().code,
        TemplateErrorCode::DiscoveryUnpriced
    );
    // A builtin cannot claim a discovery charge (nor, by construction, shed one).
    let mut claims = adsb.clone();
    claims["provenance"]["discovery_look_elsewhere_bits"] = 3.0.into();
    assert_eq!(
        load(&claims).unwrap_err().code,
        TemplateErrorCode::DiscoveryUnpriced
    );

    // A builtin with any free parameter is not proven to fix its check: searched.
    let lib = TemplateLibrary::builtin().unwrap();
    let pocsag = &lib.get("pocsag@1").unwrap().template;
    assert!(!pocsag.free.is_empty());
    assert_eq!(pocsag.check_origin(), CheckOrigin::Searched);
    let generic = &lib.get("generic-fsk-framed@1").unwrap().template;
    assert_eq!(generic.check_origin(), CheckOrigin::Searched);
}
