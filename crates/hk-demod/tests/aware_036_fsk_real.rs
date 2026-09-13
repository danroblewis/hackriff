//! AWARE-036 (unknown burst triage → bits → framing), recorded: the 915 MHz FHSS 2-FSK fixture
//! (`ism_915M_10M_l24g30a1_t42p3_1p2s`, HackRF One, 10 Msps). **Unidentified third-party
//! traffic**: framing *structure* only; payload bits are content and must never be persisted
//! or streamed under the default (fail-closed, metadata-only) class.
//!
//! 1. Pass 1: every annotated box through [`FskReceiver`] without priors; trusted C14 bursts
//!    give emitter-cluster priors (rate/deviation) and a first framing model (sync).
//! 2. Pass 2: every box with prior-led trials (cluster priors, standard rates, sync prior).
//! 3. Framing inference over all pass-2 bursts: the learned sync is `0000110001011111`
//!    (S5 truth) and is located, at the truth rate, in ≥ 90 % of the sync-truth bursts;
//!    bursts below the C14 trust floor are among them, with the prior reported. Any CRC,
//!    length field or whitening is reported, not required (S5 found none).
//! 4. Legal: records written with the default context → Decodes metadata-only (no content),
//!    Bitstream metadata-only, known status unchanged; a bits stream under that class carries
//!    only `GATED` header-only records. The SQLite files and the stream bytes are scanned for
//!    payload-derived sentinels (raw bytes in both bit orders and polarities, hex, bit strings).

mod common;

use std::collections::HashSet;
use std::io::Read;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use common::*;
use hk_demod::fsk::{
    ClusterPrior, DemodPriors, FramedRecordContext, FskBurst, FskReceiver, FskReceiverConfig,
    SeedSource, SyncPrior, bits_stream_header, publish_framed_bits, write_framed_bursts,
};
use hk_e2e::Fixture;
use hk_estimate::framing::bits::{BitOrder, bit_string, inverted, pack};
use hk_estimate::framing::{FramingConfig, infer_framing};
use hk_estimate::{Hints, SnippetConfig, SnippetRequest};
use hk_model::{ContentClass, CrcStatus, KnownStatus, Repository};
use hk_stream::{Publisher, PublisherConfig, Record, RecordFlags, StreamReader};

const AWARE_036: &str = "AWARE-036";
const NAME: &str = "ism_915M_10M_l24g30a1_t42p3_1p2s";
const SYNC: &str = "0000110001011111";

/// (raw 6-byte windows, 12-char hex strings, 48-char bit strings).
type Sentinels = (HashSet<Vec<u8>>, HashSet<Vec<u8>>, HashSet<Vec<u8>>);

/// Payload-derived sentinels: 6-byte windows (≥ 5 distinct values) of the payload packed MSB/
/// LSB first, both polarities, as raw bytes and as upper/lower hex; 48-bit bit strings.
fn sentinels(payloads: &[Vec<u8>]) -> Sentinels {
    let (mut raw, mut hex, mut bitstr) = (HashSet::new(), HashSet::new(), HashSet::new());
    for p in payloads {
        for bits in [p.clone(), inverted(p)] {
            for order in [BitOrder::MsbFirst, BitOrder::LsbFirst] {
                let bytes = pack(&bits, order);
                for w in bytes.windows(6) {
                    let distinct: HashSet<_> = w.iter().collect();
                    if distinct.len() < 5 {
                        continue;
                    }
                    raw.insert(w.to_vec());
                    let h: String = w.iter().map(|b| format!("{b:02x}")).collect();
                    hex.insert(h.clone().into_bytes());
                    hex.insert(h.to_uppercase().into_bytes());
                }
            }
            for c in bits.chunks_exact(48) {
                let changes = c.windows(2).filter(|w| w[0] != w[1]).count();
                if (12..=36).contains(&changes) {
                    bitstr.insert(bit_string(c).into_bytes());
                }
            }
        }
    }
    (raw, hex, bitstr)
}

/// Number of sentinels found in `hay`.
fn leaks(hay: &[u8], s: &Sentinels) -> usize {
    let mut n = 0;
    for (set, len) in [(&s.0, 6usize), (&s.1, 12), (&s.2, 48)] {
        if hay.len() < len || set.is_empty() {
            continue;
        }
        let windows: HashSet<&[u8]> = hay.windows(len).collect();
        n += set
            .iter()
            .filter(|x| windows.contains(x.as_slice()))
            .count();
    }
    n
}

#[test]
fn aware_036_fsk_915mhz_sync_framing_and_metadata_only() {
    let (meta, data) = fixture_or_skip!(NAME);
    let fx = Fixture::load(&meta).unwrap();
    let prov = meta_provenance(&meta);
    let center = prov.tune.center_hz;
    let ppm = fx.scenario().and_then(|s| s.f64("/clock/ppm_measured"));
    let iq = read_ci8(&data);
    let cfg = FskReceiverConfig {
        snippet: SnippetConfig {
            min_rate_hz: 1.25e6,
            ..Default::default()
        },
        hints: Hints {
            clock_ppm: ppm,
            ..Default::default()
        },
        ..Default::default()
    };
    let emissions = fx.emissions();
    let requests: Vec<SnippetRequest> = emissions
        .iter()
        .map(|b| SnippetRequest {
            start_index: b.sample_start,
            end_index: b.sample_start + b.sample_count,
            center_offset_hz: b.center_hz() - center,
            bandwidth_hz: b.bandwidth_hz(),
        })
        .collect();
    let mut rx = FskReceiver::new(cfg);
    let run = |rx: &mut FskReceiver, priors: &DemodPriors| -> Vec<FskBurst> {
        requests
            .iter()
            .map(|r| rx.run(info(0, &prov), &iq, r, priors).unwrap())
            .collect()
    };

    // ---- pass 1: no priors
    let pass1 = run(&mut rx, &DemodPriors::default());
    let cluster = ClusterPrior::from_trusted(&pass1, 0.02);
    eprintln!("[{AWARE_036}] cluster priors: {cluster:?}");
    let trusted_bits: Vec<Vec<u8>> = pass1
        .iter()
        .filter(|b| b.seed.c14_trusted && b.symbols.is_some())
        .map(|b| b.bits().to_vec())
        .collect();
    let learn = infer_framing(&trusted_bits, &FramingConfig::default());
    eprintln!(
        "[{AWARE_036}] pass 1: {} trusted bursts; sync {:?}",
        trusted_bits.len(),
        learn.model.sync.as_ref().map(|s| (&s.bits, s.found_in))
    );
    assert!(!cluster.is_empty(), "[{AWARE_036}] no trusted C14 rate");

    // ---- pass 2: prior-led trials
    let priors = DemodPriors {
        cluster: cluster.clone(),
        standard_rates: true,
        sync: SyncPrior::from_model(&learn.model),
    };
    let pass2 = run(&mut rx, &priors);
    let bits2: Vec<Vec<u8>> = pass2.iter().map(|b| b.bits().to_vec()).collect();
    let result = infer_framing(&bits2, &FramingConfig::default());
    let m = &result.model;

    let mut sync_truth = 0usize;
    let mut found = 0usize;
    let mut weak_found = 0usize;
    for (i, (b, t)) in pass2.iter().zip(&emissions).enumerate() {
        let truth_rate = t.f64("symbol_rate_bd");
        let f = &result.frames[i];
        let rate = b.symbols.as_ref().map(|s| s.rate_bd);
        eprintln!(
            "[{AWARE_036}] burst {:2} ({:9}) snr {:5.1} truth {:?} | seed {:13} rate {:?} \
             trusted {} confirmed {:?} lock {:.2} | sync bit {:?} err {:?} preamble {:?} (S5 {:?}) \
             inv {} crc {:?}",
            t.f64("burst_index").unwrap_or(-1.0),
            t.kind,
            t.f64("snr_db").unwrap_or(f64::NAN),
            truth_rate,
            b.seed.source.as_str(),
            rate,
            b.seed.c14_trusted,
            b.seed.sync_confirmed,
            b.symbols.as_ref().map_or(0.0, |s| s.lock.lock_quality),
            f.sync_bit,
            f.sync_bit_errors,
            f.preamble_bits_before_sync,
            t.f64("preamble_bits"),
            f.inverted,
            f.crc_valid,
        );
        if t.kind != "fsk-burst" {
            continue;
        }
        sync_truth += 1;
        let rate_ok =
            matches!((rate, truth_rate), (Some(r), Some(tr)) if (r / tr - 1.0).abs() < 0.01);
        if f.sync_bit.is_some() && rate_ok {
            found += 1;
            if !b.seed.c14_trusted {
                weak_found += 1;
                assert_ne!(
                    b.seed.source,
                    SeedSource::TrustedC14,
                    "[{AWARE_036}] weak burst {i} must report its prior"
                );
            }
        }
    }
    let model_json = serde_json::to_string(m).unwrap();
    eprintln!("[{AWARE_036}] framing model: {model_json}");
    eprintln!(
        "[{AWARE_036}] sync-truth bursts {sync_truth}: sync found at the truth rate in {found} \
         ({weak_found} below the C14 trust floor, prior-led); CRC {:?}; candidate {:?}; length \
         field {:?}; whitening {:?}; payload {:?}",
        m.crc
            .as_ref()
            .map(|c| (&c.algorithm, c.validated, c.tested)),
        m.crc_candidate,
        m.length_field,
        m.whitening.as_ref().map(|w| &w.name),
        m.payload
            .as_ref()
            .map(|p| (p.class, p.constant_fraction, p.mean_entropy_bits)),
    );
    let s = m.sync.as_ref().expect("sync model");
    assert_eq!(s.bits, SYNC, "[{AWARE_036}] learned sync");
    assert_eq!(s.hex.as_deref(), Some("0C5F"));
    assert!(sync_truth >= 10);
    assert!(
        found as f64 >= 0.9 * sync_truth as f64,
        "[{AWARE_036}] sync found in {found}/{sync_truth} sync-truth bursts"
    );
    assert!(
        weak_found >= 1,
        "[{AWARE_036}] no weak burst decoded by priors"
    );

    // ---- legal: metadata only, nothing payload-derived persisted or streamed
    let payloads: Vec<Vec<u8>> = (0..pass2.len())
        .filter_map(|i| result.payload(i, pass2[i].bits()))
        .map(|p| p.bits)
        .collect();
    let sent = sentinels(&payloads);
    assert!(
        sent.0.len() >= 20,
        "[{AWARE_036}] too few sentinels {}",
        sent.0.len()
    );
    let dir =
        hk_e2e::paths::target_dir().join(format!("t013-aware036-real-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("hk.sqlite");
    let framed = result
        .frames
        .iter()
        .filter(|f| f.sync_bit.is_some())
        .count();
    let stream_bytes;
    {
        let mut repo = Repository::open(&db).unwrap();
        let ctx = FramedRecordContext::default();
        assert_eq!(ctx.content_class(), ContentClass::FAIL_CLOSED);
        let w = write_framed_bursts(&mut repo, &pass2, &result, &ctx).unwrap();
        assert_eq!(w.content_class, ContentClass::MetadataOnly);
        assert_eq!(w.decode_ids.len(), framed);
        for &(_, id) in &w.decode_ids {
            let d = repo.decode(id).unwrap();
            assert_eq!(d.content_class, ContentClass::MetadataOnly);
            assert!(d.content.is_none(), "[{AWARE_036}] content persisted");
            assert!(d.metadata.get("payload_hex").is_none());
            assert!(d.metadata["payload_bits"].is_u64() || d.metadata["payload_bits"].is_null());
            if m.crc.is_none() {
                assert_eq!(d.crc_status, CrcStatus::Unknown);
            }
        }
        let bs = w.bitstream.clone().expect("bitstream descriptor");
        assert_eq!(
            repo.bitstream(bs.id).unwrap().content_class,
            ContentClass::MetadataOnly
        );
        assert!(!w.known_status_appended);
        assert_eq!(
            repo.emitter(w.emitter_id).unwrap().known_status,
            KnownStatus::Unknown
        );

        // Bits stream under the emitter's class.
        let header = bits_stream_header("bits/aware-036-915", &w, &bs, Some(center));
        assert_eq!(header.content_class, ContentClass::MetadataOnly);
        let mut publisher = Publisher::new(
            header,
            PublisherConfig {
                drain_timeout: Duration::from_secs(5),
                ..Default::default()
            },
        )
        .unwrap();
        let (ours, theirs) = UnixStream::pair().unwrap();
        let closer = ours.try_clone().unwrap();
        publisher
            .handle()
            .subscribe(
                "aware-036-test",
                ours,
                Box::new(move |_| {
                    let _ = closer.shutdown(Shutdown::Both);
                }),
            )
            .unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut t = theirs;
            t.read_to_end(&mut buf).unwrap();
            buf
        });
        let stats = publish_framed_bits(&mut publisher, &pass2, &result).unwrap();
        drop(publisher);
        stream_bytes = reader.join().unwrap();
        assert_eq!((stats.published, stats.gated), (0, framed));
        repo.checkpoint().unwrap();
    }
    let mut rd = StreamReader::new(std::io::Cursor::new(stream_bytes.clone()));
    assert_eq!(
        rd.read_header().unwrap().content_class,
        ContentClass::MetadataOnly
    );
    let mut gated = 0;
    while let Some(r) = rd.next_record().unwrap() {
        if let Record::Binary(b) = r {
            assert!(b.header.flags.contains(RecordFlags::GATED));
            assert!(b.payload.is_empty(), "[{AWARE_036}] payload streamed");
            gated += 1;
        }
    }
    assert_eq!(gated, framed);
    let stream_leaks = leaks(&stream_bytes, &sent);
    let mut db_leaks = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        db_leaks += leaks(&bytes, &sent);
    }
    eprintln!(
        "[{AWARE_036}] legal: {} raw / {} hex / {} bit-string sentinels; leaks: database {db_leaks}, \
         stream {stream_leaks} ({} bytes)",
        sent.0.len(),
        sent.1.len(),
        sent.2.len(),
        stream_bytes.len()
    );
    assert_eq!(
        db_leaks, 0,
        "[{AWARE_036}] payload-derived bytes in the database"
    );
    assert_eq!(
        stream_leaks, 0,
        "[{AWARE_036}] payload-derived bytes in the stream"
    );
    // Positive control: the scanner finds a sentinel when one is present.
    let control = sent.0.iter().next().unwrap().clone();
    assert!(leaks(&[b"xx".as_slice(), &control, b"yy"].concat(), &sent) >= 1);
    let _ = std::fs::remove_dir_all(&dir);
}
