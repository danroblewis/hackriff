//! Legal-guardrail regression (CLAUDE.md "Legal guardrails", ADR-0004): a restricted-class source
//! through the composed pipeline yields **no content and no identity in clear** in any output: the
//! data directory (SQLite DB + WAL, history tiles, SigMF recordings), every stream offered to
//! consumers, `/api/inventory` and `/api/status`.
//!
//! Non-vacuous by construction (T-027 review note):
//! - the restricted run must actually demodulate and frame bursts (CRC-valid Decodes exist) and
//!   every one of them must be withheld;
//! - a positive control replays the identical scene with the same classification rule on a
//!   non-restricted (fail-closed) source: there the sentinels **are** found in the data
//!   directory and the identity reads in clear to prove the scans can see what they look for.
//!
//! The source is tagged `hackriff:content_class: restricted-paging` explicitly, and the rule tries
//! to open it. Band-derived restriction (paging 929–932 MHz, cellular 824–894 MHz on untagged
//! recordings) is being added by a separate fix; add a band-derived case here once it merges.

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::sigmf::SigmfMeta;
use hk_model::{CrcStatus, FreqRange, InventoryIdentity, InventoryQuery, LinkTarget, Repository};
use hk_stream::StreamKind;
use serde_json::json;

use crate::common::*;

const LEGAL: &str = "legal-guardrail";

struct Outcome {
    dir: TempDir,
    summary: hk_pipeline::RunSummary,
    tap: StreamTap,
    api_inventory: Vec<u8>,
    api_rows: Vec<serde_json::Value>,
    api_status: Vec<u8>,
}

fn run_scene(fx: &hk_e2e::Fixture, class: Option<&str>, tag: &str) -> Outcome {
    let src = TempDir::new(&format!("{tag}src"));
    let meta_path = src.0.join("scene.sigmf-meta");
    let mut meta = SigmfMeta::read(&fx.meta_path).unwrap();
    if let Some(c) = class {
        meta.global.extra.insert(
            "hackriff:content_class".into(),
            serde_json::Value::String(c.into()),
        );
    }
    meta.write(&meta_path).unwrap();
    std::fs::copy(fx.data_path(), src.0.join("scene.sigmf-data")).unwrap();

    let dir = TempDir::new(tag);
    let (mut cfg, replay) = replay_config(
        &dir.0,
        &meta_path,
        json!({ "pipeline": { "classify": [{
            "freq_hz": [433.8e6, 434.1e6],
            "content_class": "unrestricted",
            "by": "test: an attempt to open the source's content"
        }] } }),
        hk_core::Pacing::Unpaced,
    );
    let tap = StreamTap::new(&dir.0, &[StreamKind::Messages]);
    tap.install(&mut cfg);
    let handle = start(cfg, replay);
    let counters = handle.counters();
    let summary = finish(handle);
    let _ = tap.socket_results();
    let server = serve_api(&dir.0, counters);
    let (api_inventory, api_rows) = api_inventory(server.local_addr());
    let (status, api_status) = api_get(server.local_addr(), "/api/status");
    assert_eq!(status, 200);
    drop(src);
    Outcome {
        dir,
        summary,
        tap,
        api_inventory,
        api_rows,
        api_status,
    }
}

/// CRC-valid decodes of the sensor, with how many kept content (gated getter).
fn sensor_decodes(repo: &Repository) -> (usize, usize) {
    let band = FreqRange::centered(433.973e6, 60e3);
    let mut valid = 0;
    let mut with_content = 0;
    for e in inventory(
        repo,
        InventoryQuery {
            freq: Some(band),
            ..InventoryQuery::default()
        },
    ) {
        for l in repo.emitter_links(e.emitter.id).unwrap() {
            if let LinkTarget::Decode(id) = l.target {
                let d = repo.decode(id).unwrap();
                if d.crc_status == CrcStatus::Valid {
                    valid += 1;
                    with_content += usize::from(d.content.is_some());
                }
            }
        }
    }
    (valid, with_content)
}

#[test]
fn legal_restricted_source_yields_no_content_or_identity_anywhere() {
    let out = synth_or_skip!(
        SynthRequest::new("fsk_burst_train")
            .seed(36)
            .param("snr_db", 20.0)
            .param("duration_s", 1.2)
    );
    let fx = out.fixture(0).unwrap();
    let truths = fx.of_kind("fsk-burst");
    let payloads: Vec<String> = truths
        .iter()
        .map(|t| t.value["frame"]["payload_hex"].as_str().unwrap().to_owned())
        .collect();
    let content = sentinels(&payloads);
    let n = truths.len();

    // --- Positive control: same scene and rule, fail-closed (not restricted) source.
    let pos = run_scene(&fx, None, "legpos");
    assert_eq!(pos.summary.source_class, "metadata-only");
    let pos_repo = repo(&pos.dir.0);
    let (pos_valid, pos_content) = sensor_decodes(&pos_repo);
    let pos_found = count_found(&all_bytes(&pos.dir.0), &content);
    // The identity the positive control shows in clear is the identity sentinel.
    let identities: Vec<String> = inventory(&pos_repo, InventoryQuery::default())
        .into_iter()
        .filter_map(|e| match e.identity {
            InventoryIdentity::Clear { identity, .. } => Some(identity.value),
            _ => None,
        })
        .collect();
    eprintln!(
        "[{LEGAL}] positive control: {pos_valid} CRC-valid decodes, {pos_content} with content, \
         {pos_found}/{} content sentinels in the data dir, identities in clear {identities:?}, \
         streams {:?}",
        content.len(),
        pos.tap.stream_ids()
    );
    assert!(
        pos_valid > 0 && pos_content > 0,
        "[{LEGAL}] positive control decoded nothing"
    );
    assert!(
        pos_found > 0,
        "[{LEGAL}] the scan cannot see stored payloads"
    );
    assert!(
        !identities.is_empty(),
        "[{LEGAL}] positive control has no identity in clear to use as a sentinel"
    );
    let identity_sentinels: Vec<Vec<u8>> =
        identities.iter().map(|i| i.clone().into_bytes()).collect();
    assert!(
        count_found(&pos.api_inventory, &identity_sentinels) > 0,
        "[{LEGAL}] the /api/inventory scan cannot see a clear identity"
    );

    // --- Restricted: paging-class source, same rule.
    let r = run_scene(&fx, Some("restricted-paging"), "legres");
    let s = &r.summary;
    assert_eq!(s.source_class, "restricted-paging");
    assert!(
        s.counter("/detect/detections") > 0,
        "[{LEGAL}] metadata flows"
    );
    assert!(
        s.counter("/chains/attached") >= 1,
        "[{LEGAL}] the FSK chain runs"
    );
    let decodes = s.counter("/chains/decodes");
    let crc = s.counter("/chains/crc_valid");
    eprintln!(
        "[{LEGAL}] restricted: {decodes} decodes ({crc} CRC-valid of {n} bursts), {} withheld, \
         {} recordings, streams {:?}",
        s.counter("/chains/content_withheld"),
        s.counter("/chains/recordings"),
        r.tap.stream_ids()
    );
    assert!(
        decodes > 0 && crc * 10 >= (n as u64) * 8,
        "[{LEGAL}] the restricted run must decode and frame bursts to be a real test"
    );
    assert_eq!(
        s.counter("/chains/content_withheld"),
        decodes,
        "[{LEGAL}] every decode withheld"
    );
    assert_eq!(s.counter("/chains/recordings"), 0, "[{LEGAL}] no recording");
    let res_repo = repo(&r.dir.0);
    let (valid, with_content) = sensor_decodes(&res_repo);
    assert!(valid > 0, "[{LEGAL}] CRC-valid Decode rows exist");
    assert_eq!(with_content, 0, "[{LEGAL}] a Decode row kept content");
    assert!(
        files_with_suffix(&r.dir.0, ".sigmf-data").is_empty()
            && files_with_suffix(&r.dir.0, ".sigmf-meta").is_empty(),
        "[{LEGAL}] a SigMF recording was written"
    );

    // Identities: withheld in the inventory and in the API rows.
    for e in inventory(&res_repo, InventoryQuery::default()) {
        assert!(
            !matches!(e.identity, InventoryIdentity::Clear { .. }),
            "[{LEGAL}] identity in clear via query_inventory: {:?}",
            e.identity
        );
    }
    for row in &r.api_rows {
        assert!(
            row["identity_value"].is_null(),
            "[{LEGAL}] /api/inventory identity in clear: {row}"
        );
    }

    // Sentinel scans over every output.
    let outputs: [(&str, Vec<u8>); 4] = [
        (
            "data directory (DB, WAL, tiles, recordings)",
            all_bytes(&r.dir.0),
        ),
        ("streams", r.tap.all_raw()),
        ("/api/inventory", r.api_inventory.clone()),
        ("/api/status", r.api_status.clone()),
    ];
    for (name, bytes) in &outputs {
        let c = count_found(bytes, &content);
        eprintln!(
            "[{LEGAL}] {name}: {} bytes, {c} content sentinels",
            bytes.len()
        );
        assert_eq!(c, 0, "[{LEGAL}] payload content in {name}");
    }
    for (name, bytes) in &outputs[1..] {
        assert_eq!(
            count_found(bytes, &identity_sentinels),
            0,
            "[{LEGAL}] identity in clear in {name}"
        );
    }
}
