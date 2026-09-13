//! SIGNAL-001 (T3 target, T4 today): ADS-B / Mode S.
//!
//! **Fixture.** The recorded 1090 MHz SigMF fixture is blocked on the antenna (T-025: the dev-Mac
//! antenna hears no ADS-B). Both tests replay the synthetic `adsb_squitter` scenario instead
//! (4 ICAO addresses × 4 DF17 squitters, CRC-24 valid, 2.4 Msps at 1090 MHz). Swap in the recorded
//! fixture when it exists.
//!
//! - [`signal_001_adsb_pipeline_and_plugin_output_plumbing`] always runs (CI has no readsb): the
//!   composed pipeline classifies the window unrestricted (ADS-B band prior) and the `adsb-readsb`
//!   coverage chain covers it; then the plugin-output half of the chain (hk-plugins `Ingest` with
//!   republish, the same wiring `chains::plugin` builds) is fed the scenario's squitters as
//!   readsb-shaped Decode rows, and the outputs are asserted exactly as for the real plugin: ≥ N
//!   CRC-valid Decodes, one aircraft Emitter per hex ICAO visible via `query_inventory` with
//!   `last_seen` updated, and the messages on a hk-stream Unix socket with correct framing.
//! - [`signal_001_adsb_readsb_plugin_chain`] runs the real readsb subprocess through the pipeline
//!   when `readsb` and the `hk-plugin-readsb` wrapper exist, else logs an explicit SKIP. All 16
//!   squitters must decode and no plugin input may drop: lossless replay waits for the plugin's
//!   queue (T-037b).
//!
//! **Short scenes, default ring (T-037b).** The plumbing test replays the 0.1 s T-015 scene and
//! the readsb test a 1 s scene (so an aircraft's squitters are further apart than the plugin's
//! decode-stamp precision), both with the default 4 s ring. The capture thread holds each tune change until the chain manager has polled
//! coverage, and chains claim their samples in the flow gate when they attach, so the coverage
//! chain attaches although the replay is shorter than the ring (before T-037b: 0 chains, 0
//! decodes, worked around here with a 2 s scene and `ring_s: 0.5`).
//!
//! **Blind replay.** The pipeline replays a copy of the scene without its SigMF annotations
//! (`blind::blind_config`); the aircraft truth stays in the test. The readsb test also checks the
//! T-039 family step for plugin decodes: every aircraft emitter's top explanation is ADS-B.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hk_e2e::{Fixture, SynthRequest};
use hk_model::{
    ContentClass, CrcStatus, Decode, DecodeId, DecodedIdentity, FreqRange, IdentityScheme,
    InventoryIdentity, InventoryQuery, Region, Timestamp,
};
use hk_pipeline::builtin_chains;
use hk_plugins::Ingest;
use hk_stream::{Publisher, PublisherConfig, Record, StreamHeader, StreamKind};
use serde_json::json;

use crate::blind::{BlindConfig, BlindSource, blind_config};
use crate::common::*;

const SIGNAL_001: &str = "SIGNAL-001";
/// Plugin decodes of the 16 squitters (lossless replay drops no plugin input).
const PLUGIN_DECODES: usize = 16;
const DECODE_STREAM: &str = "decodes/readsb";

/// The scene: 4 aircraft × 4 squitters over `duration_s`.
fn scenario(duration_s: f64) -> Option<Fixture> {
    match SynthRequest::new("adsb_squitter")
        .seed(7)
        .param("duration_s", duration_s)
        .param("messages_per_aircraft", 4)
        .generate()
    {
        Ok(out) => Some(out.fixture(0).unwrap()),
        Err(e) if e.is_unavailable() && !hk_e2e::synth::require_synth() => {
            eprintln!("SKIP {SIGNAL_001}: {e}");
            None
        }
        Err(e) => panic!("[{SIGNAL_001}] synthetic scenario generation failed: {e}"),
    }
}

fn truth_icaos(fx: &Fixture) -> BTreeSet<String> {
    fx.scenario().unwrap().value["aircraft"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["icao"].as_str().unwrap().to_owned())
        .collect()
}

/// The shared SIGNAL-001 output assertions (Repository, inventory, stream framing).
fn assert_adsb_outputs(
    dir: &Path,
    streams: &[(String, TapResult)],
    icaos: &BTreeSet<String>,
    min_decodes: usize,
) {
    let repo = repo(dir);
    // CRC-valid Decode rows per ICAO (ADS-B is unrestricted: the default getter shows them).
    let mut per_icao: BTreeMap<String, Vec<Decode>> = BTreeMap::new();
    for icao in icaos {
        let id = DecodedIdentity {
            scheme: IdentityScheme::AdsbIcao,
            value: icao.clone(),
        };
        let valid: Vec<Decode> = repo
            .decodes_for_identity(&id)
            .unwrap()
            .into_iter()
            .filter(|d| d.crc_status == CrcStatus::Valid)
            .collect();
        per_icao.insert(icao.clone(), valid);
    }
    let total: usize = per_icao.values().map(Vec::len).sum();
    eprintln!(
        "[{SIGNAL_001}] CRC-valid decodes per ICAO: {:?}",
        per_icao
            .iter()
            .map(|(k, v)| (k, v.len()))
            .collect::<Vec<_>>()
    );
    assert!(
        total >= min_decodes,
        "[{SIGNAL_001}] {total} CRC-valid decodes < {min_decodes}"
    );

    // One aircraft Emitter per ICAO, identity in clear, last_seen at the newest decode.
    let entries = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..InventoryQuery::default()
        },
    );
    assert_eq!(
        entries.len(),
        icaos.len(),
        "[{SIGNAL_001}] one emitter per ICAO: {entries:?}"
    );
    for (icao, decodes) in &per_icao {
        assert!(
            !decodes.is_empty(),
            "[{SIGNAL_001}] ICAO {icao} not decoded"
        );
        let e = entries
            .iter()
            .find(|e| {
                matches!(&e.identity, InventoryIdentity::Clear { identity, class }
                    if identity.value == *icao && *class == ContentClass::Unrestricted)
            })
            .unwrap_or_else(|| panic!("[{SIGNAL_001}] no visible emitter for {icao}"));
        assert!(
            icao.len() == 6 && icao.chars().all(|c| c.is_ascii_hexdigit()),
            "[{SIGNAL_001}] hex identity {icao}"
        );
        let newest = decodes.iter().map(|d| d.t).max().unwrap();
        let oldest = decodes.iter().map(|d| d.t).min().unwrap();
        assert_eq!(
            e.emitter.last_seen, newest,
            "[{SIGNAL_001}] {icao}: last_seen follows the newest decode"
        );
        assert!(e.emitter.first_seen <= oldest);
        if decodes.len() >= 2 {
            assert!(
                e.emitter.last_seen > e.emitter.first_seen,
                "[{SIGNAL_001}] {icao}: last_seen updated by later sightings"
            );
        }
    }

    // The stream-output socket: header, framing, seq order, every stored decode delivered.
    let (_, tap) = streams
        .iter()
        .find(|(id, _)| id == DECODE_STREAM)
        .unwrap_or_else(|| {
            panic!(
                "[{SIGNAL_001}] no {DECODE_STREAM} stream: {:?}",
                streams.iter().map(|s| &s.0).collect::<Vec<_>>()
            )
        });
    assert!(
        tap.error.is_none(),
        "[{SIGNAL_001}] reader: {:?}",
        tap.error
    );
    let header = tap.header.as_ref().unwrap();
    assert_eq!(header.kind, StreamKind::Messages);
    assert_eq!(header.message_schema.as_deref(), Some("hackriff.decode/1"));
    assert_eq!(header.content_class, ContentClass::Unrestricted);
    let mut last_seq = None;
    let mut delivered = BTreeSet::new();
    let mut messages = 0;
    for r in &tap.records {
        match r {
            Record::Message(m) => {
                if let Some(prev) = last_seq {
                    assert!(m.seq > prev, "[{SIGNAL_001}] seq {} after {prev}", m.seq);
                }
                last_seq = Some(m.seq);
                messages += 1;
                assert!(!m.gated);
                let v = &m.value;
                if messages == 1 {
                    eprintln!("[{SIGNAL_001}] first stream message: {v}");
                }
                let icao = v["identity"]["value"]
                    .as_str()
                    .or_else(|| v["metadata"]["icao"].as_str())
                    .unwrap_or_else(|| panic!("[{SIGNAL_001}] message without an ICAO: {v}"));
                delivered.insert(icao.to_owned());
            }
            Record::Dropped(d) => panic!("[{SIGNAL_001}] consumer dropped {d:?}"),
            other => panic!("[{SIGNAL_001}] unexpected record {other:?}"),
        }
    }
    eprintln!("[{SIGNAL_001}] socket consumer: {messages} framed messages, ICAOs {delivered:?}");
    assert_eq!(
        messages, total,
        "[{SIGNAL_001}] every stored decode streamed"
    );
    assert_eq!(
        &delivered, icaos,
        "[{SIGNAL_001}] every aircraft on the stream"
    );
}

#[test]
fn signal_001_adsb_pipeline_and_plugin_output_plumbing() {
    let Some(fx) = scenario(0.1) else { return };
    let icaos = truth_icaos(&fx);
    let msgs = fx.of_kind("adsb-df17");
    assert_eq!(msgs.len(), 16);
    let t0 = hk_core::source::sigmf_replay::parse_sigmf_datetime(
        fx.meta.captures[0].datetime.as_deref().unwrap(),
    )
    .unwrap();

    // Composed pipeline over the scene, without the plugin chain (no subprocess here).
    let BlindConfig {
        dir,
        mut cfg,
        replay,
        src: _src,
    } = blind_config(&fx.meta_path, "s001", BlindSource::default(), json!({}));
    assert_eq!(
        replay.class,
        ContentClass::Unrestricted,
        "[{SIGNAL_001}] ADS-B band prior"
    );
    let adsb = builtin_chains()
        .into_iter()
        .find(|c| c.id == "adsb-readsb")
        .unwrap();
    assert!(
        adsb.covered_by(replay.info.center_hz, 0.9 * replay.info.sample_rate_hz),
        "[{SIGNAL_001}] the adsb-readsb coverage chain covers the replay window"
    );
    cfg.settings.chains = Some(
        builtin_chains()
            .into_iter()
            .filter(|c| c.id != "adsb-readsb")
            .collect(),
    );
    let s = finish(start(cfg, replay));
    assert_eq!(s.source_class, "unrestricted");
    assert_eq!(s.always_on_lost_samples, 0);
    assert!(s.counter("/readers/detect/frames") > 0);
    let dets = repo(&dir.0)
        .detections_in_region(&Region::new(FreqRange::centered(1090e6, 2.4e6), ever()))
        .unwrap();
    eprintln!(
        "[{SIGNAL_001}] {} detections in the 1090 MHz window",
        dets.len()
    );

    // Plugin-output half of the chain: Ingest + republish, as chains::plugin wires it.
    let class = hk_stream::gate::clamp(ContentClass::Unrestricted, ContentClass::Unrestricted);
    let mut header = StreamHeader::new(
        DECODE_STREAM,
        StreamKind::Messages,
        class,
        "hk-plugins:readsb@0.1.0",
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let publisher = Publisher::new(
        header.clone(),
        PublisherConfig {
            queue_bytes: 4 << 20,
            ..PublisherConfig::default()
        },
    )
    .unwrap();
    let tap = StreamTap::new(&dir.0, &[StreamKind::Messages]);
    tap.offer(&header, publisher.handle());
    let mut ingest = Ingest::with_republish(repo(&dir.0), publisher);
    let mut ordered: Vec<_> = msgs.iter().collect();
    ordered.sort_by(|a, b| a.t_start_s.total_cmp(&b.t_start_s));
    for m in ordered {
        let hex = m.str("message_hex").unwrap();
        let icao = hex[2..8].to_owned();
        assert!(icaos.contains(&icao), "{hex}");
        let d = Decode {
            id: DecodeId::new(),
            demodulation_ref: None,
            recording_ref: None,
            decoder_id: "readsb".into(),
            decoder_version: "acceptance-stub".into(),
            frame_model: "adsb-df17".into(),
            metadata: json!({ "icao": icao, "df": 17 }),
            content: Some(json!({ "message_hex": hex })),
            crc_status: CrcStatus::Valid,
            identity: Some(DecodedIdentity {
                scheme: IdentityScheme::AdsbIcao,
                value: icao,
            }),
            content_class: ContentClass::Unrestricted,
            t: t0.saturating_add_nanos((m.t_start_s * 1e9) as i64),
        };
        ingest
            .store_decode(d, None, None, Some(1090e6), Some(2.0e6))
            .unwrap();
    }
    let stats = ingest.stats();
    assert_eq!(stats.decodes_stored, 16);
    ingest.take_publisher().unwrap().finish();
    drop(ingest);
    let _: Timestamp = t0;
    assert_adsb_outputs(&dir.0, &tap.socket_results(), &icaos, 16);
}

#[test]
fn signal_001_adsb_readsb_plugin_chain() {
    if !readsb_available() {
        eprintln!(
            "SKIP {SIGNAL_001} readsb plugin chain: readsb not found on PATH (CI has none; the \
             pipeline + plugin-output plumbing test still ran)"
        );
        return;
    }
    // 1 s: an aircraft's squitters ~250 ms apart, well beyond the plugin decode stamp precision
    // (≤ ~40 ms, hk-plugin-readsb "Timestamps"), so `last_seen` visibly moves; still far shorter
    // than the default 4 s ring (the 0.1 s attach case is `hk-pipeline/tests/signal_001_readsb.rs`).
    let Some(fx) = scenario(1.0) else { return };
    let icaos = truth_icaos(&fx);
    let BlindConfig {
        dir,
        mut cfg,
        replay,
        src: _src,
    } = blind_config(&fx.meta_path, "s001r", BlindSource::default(), json!({}));
    if !cfg
        .plugin_dirs
        .iter()
        .any(|d| d.join("hk-plugin-readsb").is_file())
    {
        eprintln!(
            "SKIP {SIGNAL_001} readsb plugin chain: hk-plugin-readsb is not built next to the \
             test binary (`cargo build -p hk-plugins --bins`; `just acceptance` does this)"
        );
        return;
    }
    let tap = StreamTap::new(&dir.0, &[StreamKind::Messages]);
    tap.install(&mut cfg);
    let s = finish(start(cfg, replay));
    assert_eq!(
        s.counter("/chains/attached"),
        1,
        "[{SIGNAL_001}] coverage chain"
    );
    let decodes = s.counter("/chains/plugin_decodes") as usize;
    eprintln!(
        "[{SIGNAL_001}] readsb: {decodes} of 16 squitters decoded, {} plugin records dropped",
        s.counter("/chains/plugin_dropped")
    );
    assert_eq!(
        s.counter("/chains/plugin_dropped"),
        0,
        "[{SIGNAL_001}] lossless replay drops no plugin input"
    );
    assert_eq!(
        decodes, PLUGIN_DECODES,
        "[{SIGNAL_001}] readsb decodes {decodes} of {PLUGIN_DECODES}"
    );
    assert_adsb_outputs(&dir.0, &tap.socket_results(), &icaos, PLUGIN_DECODES);

    // T-039 family step for plugin decodes (T-037b): the readsb chain classifies the emitters its
    // decodes resolved to, so each aircraft's top ranked explanation is ADS-B.
    let repo = repo(&dir.0);
    let aircraft = inventory(
        &repo,
        InventoryQuery {
            identity_scheme: Some(IdentityScheme::AdsbIcao),
            ..InventoryQuery::default()
        },
    );
    assert_eq!(aircraft.len(), icaos.len());
    for e in &aircraft {
        let ranked = hk_pipeline::explanations(&repo, e.emitter.id).unwrap();
        assert_eq!(
            ranked.first().map(|x| x.service.as_str()),
            Some("adsb"),
            "[{SIGNAL_001}] top explanation of {:?}: {ranked:?}",
            e.identity
        );
    }
}
