//! T-211 (ADR-0016 §2): inventory rows expose the classification that sets `family` (arbitration
//! rank), the newer lower-ranked row as `latest_classification`, and the M3 fields (`taxonomy`,
//! `stage`, `arb_rank`, `coarse`, `class`, `top`, `entropy_norm`, `flags`), null on pre-M3 rows.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hk_api::{ApiState, Server, ServerConfig, Token};
use hk_model::classify::{
    ArbRank, CLASSIFICATION_SCHEMA, ClassCall, ClassProvenance, Classification, HK_MOD_V1, LabelP,
    Stage, SuspectFlags, TaxonomyRef, UNKNOWN, entropy_norm,
};
use hk_model::{Repository, Timestamp};
use serde_json::{Value, json};

#[allow(dead_code)]
#[path = "support/seed_inventory.rs"]
mod seed;

const TOKEN: &str = "t211-inventory-token-0123456789abcdef";
const T0: i64 = seed::DEFAULT_T0_S;

fn get(addr: SocketAddr, path: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap();
    (status, serde_json::from_slice(&raw[split + 4..]).unwrap())
}

fn row(v: &Value, id: impl ToString) -> Value {
    let id = id.to_string();
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(id))
        .unwrap_or_else(|| panic!("row {id} missing"))
        .clone()
}

fn listed(addr: SocketAddr, path: &str, id: impl ToString) -> bool {
    let (st, v) = get(addr, path);
    assert_eq!(st, 200, "{path}: {v}");
    let id = id.to_string();
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["id"] == json!(id))
}

fn lp(label: &str, p: f64) -> LabelP {
    LabelP {
        label: label.into(),
        p,
    }
}

fn m3(family: &str, class: Option<&str>, stage: Stage, t_s: i64) -> Classification {
    let (posterior, confidence) = if family == UNKNOWN {
        (vec![lp("analog", 0.3), lp(UNKNOWN, 0.7)], 0.7)
    } else {
        (vec![lp(family, 0.8), lp(UNKNOWN, 0.2)], 0.8)
    };
    Classification {
        schema: CLASSIFICATION_SCHEMA,
        t: Timestamp::from_unix_nanos(t_s * 1_000_000_000),
        taxonomy: TaxonomyRef::current(),
        input: None,
        coarse: HK_MOD_V1.coarse_of(family).unwrap(),
        entropy_norm: entropy_norm(&posterior, HK_MOD_V1.families.len() + 1),
        likelihood: posterior.clone(),
        posterior,
        prior: None,
        family: family.into(),
        confidence,
        class: class.map(|label| ClassCall {
            label: label.into(),
            p: 0.9,
            dist: Vec::new(),
            stage,
        }),
        open_set_score: 1.0 - confidence,
        stage,
        provenance: ClassProvenance {
            rules: "hk-classify/tree@1".into(),
            // Determinate (T-292): this `m3` builder stands for an M3 row a current writer
            // produced, not the pre-T-290 `FEATURES_VERSION_INDETERMINATE` marker. hk-api doesn't
            // depend on hk-classify, so it can't name `hk_classify::FEATURES_VERSION` directly.
            features_version: 2,
            features_ref: None,
            ml: None,
            snr_db: Some(24.0),
            snr_gate_db: 10.0,
            gated: false,
            thresholds: "thresholds@1".into(),
            suspect: SuspectFlags::default(),
            power_mode: None,
        },
        flags: Vec::new(),
        reasons: Vec::new(),
    }
}

#[test]
fn t211_inventory_rows_expose_the_arbitrated_classification_and_m3_fields() {
    let mut repo = Repository::open_in_memory().unwrap();
    let s = seed::seed(&mut repo, T0).unwrap();
    // A lock-verified chain label, then a later pre-sync `unknown`: the unknown never hides it.
    repo.record_classification(
        s.carrier,
        &m3("analog", Some("nbfm"), Stage::Chain, T0 + 10),
        ArbRank::LockVerified,
    )
    .unwrap();
    repo.record_classification(
        s.carrier,
        &m3(UNKNOWN, None, Stage::FeatureTree, T0 + 20),
        ArbRank::Classifier,
    )
    .unwrap();
    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    let addr = server.local_addr();

    let (st, v) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{v}");
    let r = row(&v, s.carrier);
    assert_eq!(r["family"], json!("analog"));
    let c = &r["classification"];
    assert_eq!(c["family"], json!("analog"), "{r}");
    assert_eq!(c["stage"], json!("chain"));
    assert_eq!(c["arb_rank"], json!(2));
    assert_eq!(c["taxonomy"], json!("hk-mod@1"));
    assert_eq!(c["coarse"], json!("analog"));
    assert_eq!(
        c["class"],
        json!({"label": "nbfm", "p": 0.9, "stage": "chain"})
    );
    assert_eq!(
        c["top"],
        json!([{"label": "analog", "p": 0.8}, {"label": "unknown", "p": 0.2}])
    );
    assert!(
        c["entropy_norm"]
            .as_f64()
            .is_some_and(|h| h > 0.0 && h < 1.0)
    );
    assert_eq!(c["flags"], json!([]));
    assert_eq!(c["model_version"], json!("hk-classify/tree@1"));
    let l = &r["latest_classification"];
    assert_eq!(l["family"], json!("unknown"), "{r}");
    assert_eq!(l["stage"], json!("feature-tree"));
    assert_eq!(l["arb_rank"], json!(3));
    assert_eq!(l["coarse"], json!("unknown"));
    assert!(l["class"].is_null());

    // The family filter agrees with the rank.
    assert!(listed(addr, "/api/inventory?family=analog", s.carrier));
    assert!(!listed(addr, "/api/inventory?family=unknown", s.carrier));

    // Pre-M3 rows: derived stage and rank, M3 fields null, no separate latest.
    let rds = row(&v, s.rds);
    let rc = &rds["classification"];
    assert_eq!(rc["family"], json!("wfm"), "{rds}");
    assert_eq!(rc["stage"], json!("chain"));
    assert_eq!(rc["arb_rank"], json!(3));
    for field in [
        "taxonomy",
        "coarse",
        "class",
        "top",
        "entropy_norm",
        "flags",
    ] {
        assert!(rc[field].is_null(), "{field}: {rds}");
    }
    assert!(rds["latest_classification"].is_null(), "{rds}");
    let legacy = row(&v, s.legacy);
    assert!(legacy["classification"].is_null(), "{legacy}");
    assert!(legacy["latest_classification"].is_null(), "{legacy}");

    // One entry: the same shape.
    let (st, one) = get(addr, &format!("/api/inventory/{}", s.carrier));
    assert_eq!(st, 200, "{one}");
    assert_eq!(one["classification"], r["classification"]);
    assert_eq!(one["latest_classification"], r["latest_classification"]);
}

/// **T-886: the classifier's posterior is served on a rank-3-tied emitter, not hidden by the
/// restatement that keeps the chain's family.**
///
/// T-878 resolved the tie between an unlocked demodulator-chain label and the C15 row at the same
/// rank by writing the posterior and then **re-appending the chain's own label** after it, so
/// "latest among equals" still leaves the family with the chain. The newest row is then a copy of
/// the current one, and a reader that served "the latest row when it differs from the current"
/// answered `null` — losing the posterior for exactly the emitters the classifier had measured.
/// Both routes now serve the latest row **unlike** the current one.
#[test]
fn t886_a_restated_chain_label_does_not_hide_the_classifiers_posterior() {
    let mut repo = Repository::open_in_memory().unwrap();
    let s = seed::seed(&mut repo, T0).unwrap();
    let id = s.carrier;
    // The write order of `hk_pipeline::classify::record` on the rank-3 tie.
    let chain = m3("analog", Some("nbfm"), Stage::Chain, T0 + 10);
    repo.record_classification(id, &chain, ArbRank::Classifier)
        .unwrap();
    let keeps = repo.current_classification(id).unwrap().unwrap();
    let posterior = m3(UNKNOWN, None, Stage::FeatureTree, T0 + 20);
    repo.record_classification(id, &posterior, ArbRank::Classifier)
        .unwrap();
    repo.append_classification_ranked(id, &keeps.classification, keeps.stage, keeps.arb_rank)
        .unwrap();

    let config = ServerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        Token::from_config(TOKEN).unwrap(),
    );
    let state = ApiState {
        inventory: Some(Arc::new(Mutex::new(repo))),
        ..ApiState::default()
    };
    let server = Server::start(config, state).unwrap();
    let addr = server.local_addr();

    let (st, v) = get(addr, "/api/inventory");
    assert_eq!(st, 200, "{v}");
    let r = row(&v, id);
    // The chain keeps the family, exactly as T-878 decided.
    assert_eq!(r["family"], json!("analog"), "{r}");
    assert_eq!(r["classification"]["stage"], json!("chain"), "{r}");
    // ... and the measurement made beside it is still readable.
    let l = &r["latest_classification"];
    assert_eq!(l["stage"], json!("feature-tree"), "{r}");
    assert_eq!(l["family"], json!("unknown"), "{r}");
    assert_eq!(l["arb_rank"], json!(3), "{r}");

    // The per-emitter route follows the same rule, with the full posterior. Its
    // `classification` is `null` here because the restated row is legacy-shaped (T-878 re-appends
    // family, confidence and open-set score, not a distribution), which is the documented reading
    // of `null` — "that row carries no M3 detail" — and exactly why this route tells a client to
    // read `latest` first.
    let (st, c) = get(addr, &format!("/api/inventory/{id}/classification"));
    assert_eq!(st, 200, "{c}");
    assert!(c["classification"].is_null(), "{c}");
    assert_eq!(c["latest"]["stage"], json!("feature-tree"), "{c}");
    assert_eq!(c["latest"]["family"], json!("unknown"), "{c}");
    assert!(
        c["latest"]["posterior"].is_array(),
        "the posterior itself is served: {c}"
    );
}
