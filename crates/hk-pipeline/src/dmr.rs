//! Conventional DMR (Tier II) identification on a measured region, and the headers it publishes
//! (T-989, C21/C23).
//!
//! # The gap this closes
//!
//! `chains::trunk` hunts **trunking control channels**: it measures raster occupancy, demodulates
//! the candidates and confirms on frame sync plus CRC, and what it can name is a *system* — P25,
//! DMR Tier III, NXDN Type-C. A conventional DMR repeater has no control channel, so that hunt
//! never reaches it, and until this module an explorer window watching a +24 dB DMR repeater at
//! 464.6125 MHz (42 base-station data syncs in ten seconds by an independent oracle) saw every
//! burst arrive with **no classification at all**.
//!
//! This module asks the question the other path cannot: given any narrowband emission somebody
//! already measured, *is it DMR?* — answered from the air interface alone by
//! [`hk_detect::dmr_tier2`], never from a band plan, an allocation or a frequency lookup.
//!
//! # Where it runs, and what it costs
//!
//! On the **classifying chain's** own thread ([`crate::chains::classify`]), over the same box the
//! C15 classifier chose, off the ring and the DSP readers. Per region the cost is one snippet
//! extraction, up to [`DEMOD_INTEGRATE_LADDER`]`.len()` 4FSK demodulations of that snippet, and
//! one sync scan — bounded by the chain's own `window_s`, which ships at 0.25 s.
//!
//! Two gates keep that off every region:
//!
//! - **Bandwidth.** A DMR channel is 12.5 kHz. A region measured outside
//!   [`DMR_BANDWIDTH_HZ`] cannot be one, so nothing is spent on it. This is physics, not a band
//!   plan: it says what the emission's own occupied bandwidth is, and it is the reason a WFM
//!   station or a 2 MHz data carrier never reaches the demodulator here.
//! - **Nothing else.** In particular *not* the classifier's verdict: the emissions this exists for
//!   are exactly the ones the classifier abstained on (`classification: null` was the explorer's
//!   finding), so a run that could not classify a burst still asks whether it is DMR. The sync
//!   search is itself the 4FSK-at-4800-Bd test, at a ~1e-11 false-alarm rate per position, so
//!   asking costs a demodulation and can never answer wrongly for lack of a prior.
//!
//! # Metadata only
//!
//! What is written is signalling — the verdict, the colour code, the slot, the data types seen,
//! and a header's own addresses — at [`ContentClass::MetadataOnly`], the class the trunking chain
//! already runs under. Voice payload is skipped by position, there is no vocoder in the workspace,
//! and a privacy-indicator header is recorded as *present* with nothing read out of it. Nothing
//! here decrypts anything and nothing ever says *clear* (T-270's rule).

use hk_detect::dmr_tier2::{self, Header, Tier2Scan};
use hk_dsp::{InputInfo, IqSample};
use hk_estimate::{SnippetExtractor, SnippetRequest};
use hk_model::{
    Annotation, AnnotationAuthor, AnnotationId, AnnotationKind, AnnotationTarget, ContentClass,
    CrcStatus, EmitterId, RepoError, Repository, Timestamp,
};
use hk_stream::{MessageRecord, Publisher, PublisherConfig, StreamError, StreamHeader, StreamKind};
use serde_json::json;

use crate::run::Shared;
use crate::stats::inc;

/// The decoder id this evidence is filed under (`crate::family`'s map sends it to
/// `public-safety`).
pub const DMR_TIER2_DECODER_ID: &str = "dmr-tier2";

/// Version of this identifier, recorded as an annotation's `author_ref`.
pub const DMR_TIER2_VERSION: &str = "hk-detect/dmr-tier2@0.1.0";

/// Occupied bandwidths a DMR emission can have, Hz.
///
/// DMR is a 12.5 kHz channel; the window is wide either side of it because what is measured here
/// is a *detection box*, whose edges come from a CFAR extent and not from a channel plan. It is a
/// spend gate — everything that passes it still has to produce sync words — so it is set to
/// exclude what obviously cannot be DMR rather than to admit exactly what is.
pub const DMR_BANDWIDTH_HZ: [f64; 2] = [4_000.0, 30_000.0];

/// A DMR channel's width, Hz: the least the snippet's DDC may pass.
pub const DMR_CHANNEL_HZ: f64 = 12_500.0;

/// Samples per symbol the snippet is extracted at, at least.
pub const DMR_MIN_SAMPLES_PER_SYMBOL: f64 = 8.0;

/// Integrate-and-dump fractions tried, best sync count kept. The same ladder and the same reason
/// as the trunking chain's (T-628): one fraction suits a clean signal and another a noisy one, and
/// which is which cannot be known before demodulating.
pub const DEMOD_INTEGRATE_LADDER: [f64; 2] = [0.5, 0.8];

/// Confidence the decoder evidence carries. The same 0.97 the trunking decoders use, and for the
/// same reason (T-546): what matched is a CRC- and FEC-checked air interface, and nothing else
/// transmits DMR bursts.
pub const DMR_TIER2_CONFIDENCE: f64 = 0.97;

/// Whether a region's measured bandwidth could be DMR's 12.5 kHz channel ([`DMR_BANDWIDTH_HZ`]).
///
/// The spend gate, exposed so the caller can count the regions it actually scanned apart from the
/// ones it never looked at.
pub fn bandwidth_admits(bandwidth_hz: f64) -> bool {
    (DMR_BANDWIDTH_HZ[0]..=DMR_BANDWIDTH_HZ[1]).contains(&bandwidth_hz)
}

/// Scans one measured region for conventional DMR.
///
/// `None` when the region cannot be DMR by bandwidth, when the snippet or the 4FSK demodulation
/// failed, or when the scan found fewer than `MIN_TIER2_SYNCS` syncs — in which case *nothing is
/// claimed about the region*, which is not the same as claiming it is not DMR.
pub fn identify<T: IqSample>(
    info: InputInfo<'_>,
    iq: &[T],
    request: &SnippetRequest,
) -> Option<Tier2Scan> {
    if !bandwidth_admits(request.bandwidth_hz) {
        return None;
    }
    let snippet = SnippetExtractor::new(hk_estimate::SnippetConfig {
        // A DMR channel is 12.5 kHz wide whatever a CFAR extent measured, so the DDC has to pass
        // at least that, and the demodulator needs samples per symbol the bandwidth rule alone
        // would not give on a narrow box — the same reason C14 raises `min_rate_hz`.
        min_bandwidth_hz: DMR_CHANNEL_HZ,
        min_rate_hz: DMR_MIN_SAMPLES_PER_SYMBOL * dmr_tier2::DMR_SYMBOL_RATE_BD,
        ..Default::default()
    })
    .extract(info, iq, request)
    .ok()?;
    let scan = demodulate_best(&snippet.samples, snippet.sample_rate_hz)?;
    scan.identified().then_some(scan)
}

/// Demodulates once per [`DEMOD_INTEGRATE_LADDER`] entry and keeps the scan with the most sync
/// hits (ties by the fewest dibit errors). `None` when no entry demodulated at all.
fn demodulate_best(samples: &[num_complex::Complex32], rate_hz: f64) -> Option<Tier2Scan> {
    let mut best: Option<Tier2Scan> = None;
    for integrate_fraction in DEMOD_INTEGRATE_LADDER {
        let cfg = hk_demod::fsk::C4fmConfig {
            symbol_rate_bd: dmr_tier2::DMR_SYMBOL_RATE_BD,
            integrate_fraction,
            ..Default::default()
        };
        let Ok(symbols) = hk_demod::fsk::C4fmDemod::new(cfg).demodulate(samples, rate_hz, 0.0)
        else {
            continue;
        };
        let scan = dmr_tier2::scan(&symbols.dibits);
        let better = match &best {
            None => true,
            Some(b) => {
                let (n, e) = (scan.hits.len(), errors(&scan));
                let (bn, be) = (b.hits.len(), errors(b));
                n > bn || (n == bn && e < be)
            }
        };
        if better {
            best = Some(scan);
        }
    }
    best
}

/// Total dibit errors across a scan's sync hits, the tie-break between two equal hit counts.
fn errors(scan: &Tier2Scan) -> usize {
    scan.hits.iter().map(|h| h.errors).sum()
}

/// What one header says, as metadata: never payload, and never key material.
fn header_json(h: &Header) -> serde_json::Value {
    match h {
        Header::VoiceLc(lc) | Header::TerminatorLc(lc) => json!({
            "header": h.name(),
            "flco": lc.flco,
            "fid": lc.fid,
            "destination": lc.destination,
            "source": lc.source,
            // Carried verbatim; nothing decides anything from it, and in particular no
            // encryption state (T-270).
            "service_options": lc.service_options,
        }),
        Header::Data(d) => json!({
            "header": h.name(),
            "group": d.group,
            "dpf": d.dpf,
            "sap": d.sap,
            "destination": d.destination,
            "source": d.source,
            "blocks_to_follow": d.blocks_to_follow,
        }),
        Header::Csbk(c) => json!({
            "header": h.name(),
            "csbko": c.csbko,
            "fid": c.fid,
            "opcode_name": hk_detect::trunk::csbko_name(c.csbko),
        }),
        // Present, and that is the whole finding: its payload is an initialisation vector and key
        // identification, and this build reads none of it.
        Header::PrivacyIndicator => json!({ "header": h.name(), "encrypted": true }),
    }
}

/// The scan as metadata for the emitter's row.
pub fn scan_json(scan: &Tier2Scan) -> serde_json::Value {
    let (syncs, slots) = scan.sync_counts();
    json!({
        "protocol": "dmr-tier2",
        "verdict": scan.verdict(),
        "colour_code": scan.colour_code,
        "colour_code_agreements": scan.colour_code_agreements,
        "colour_code_disagreements": scan.colour_code_disagreements,
        "syncs": syncs,
        "slots": slots,
        "sync_words": scan.sync_names().iter().map(|(n, c)| json!({"sync": n, "count": c}))
            .collect::<Vec<_>>(),
        "grid_consistent": scan.grid_consistent,
        "slot_numbers": scan.bursts.iter().filter_map(|b| b.cach.map(|c| c.tdma_channel))
            .collect::<std::collections::BTreeSet<u8>>(),
        "data_types": scan.bursts.iter().filter_map(|b| b.slot_type.and_then(|s| s.data_type.name()))
            .collect::<std::collections::BTreeSet<&str>>(),
        "headers": scan.headers().map(header_json).collect::<Vec<_>>(),
        "bptc_failed": scan.bptc_failed,
        "check_failed": scan.check_failed,
        "decoder": DMR_TIER2_VERSION,
    })
}

/// Writes the identification against `emitter`: decoder evidence for the family map, and the
/// verdict as a metadata-only annotation a row can show.
///
/// The annotation's `value` is the verdict line itself (`DMR Tier II, CC 3, sync 8/9`), so a
/// reader sees what was decided and what it rests on without parsing anything.
pub fn record(
    repo: &mut Repository,
    emitter: EmitterId,
    scan: &Tier2Scan,
    t: Timestamp,
) -> Result<Option<AnnotationId>, RepoError> {
    let Some(verdict) = scan.verdict() else {
        return Ok(None);
    };
    crate::family::record_decoder_evidence(
        repo,
        emitter,
        DMR_TIER2_DECODER_ID,
        DMR_TIER2_CONFIDENCE,
        t,
    )?;
    let id = repo.live_emitter_id(emitter)?;
    let a = Annotation {
        id: AnnotationId::new(),
        target: AnnotationTarget::Emitter(id),
        author: AnnotationAuthor::Decoder,
        author_ref: DMR_TIER2_VERSION.into(),
        kind: AnnotationKind::Label,
        value: verdict,
        metadata: json!({ "dmr_tier2": scan_json(scan) }),
        content: None,
        confidence: DMR_TIER2_CONFIDENCE,
        supersedes: None,
        content_class: ContentClass::MetadataOnly,
        t,
        exported: false,
    };
    repo.insert_annotation(&a)?;
    Ok(Some(a.id))
}

/// Publishes the scan's checked headers on a `messages` stream.
///
/// Only headers whose **own** check passed are here: a full LC's RS parity, a data header's CRC, a
/// CSBK's CRC. A block whose BPTC or check refused it is counted on the run
/// (`dmr_headers_refused`) and never published, so a reader of this stream is reading frames the
/// air interface itself validated.
pub(crate) fn publish_headers(
    shared: &Shared,
    emitter: EmitterId,
    scan: &Tier2Scan,
    center_hz: f64,
    bandwidth_hz: f64,
    t: Timestamp,
) {
    let c = &shared.counters.chains;
    if scan.headers().next().is_none() {
        return;
    }
    let mut header = StreamHeader::new(
        format!("messages/dmr-tier2/{emitter}"),
        StreamKind::Messages,
        ContentClass::MetadataOnly,
        "hk-pipeline:dmr-tier2",
    );
    header.center_hz = Some(center_hz);
    header.bandwidth_hz = Some(bandwidth_hz);
    header.emitter_id = Some(emitter);
    let mut publisher = match Publisher::new(header.clone(), PublisherConfig::default()) {
        Ok(p) => p,
        Err(e) => {
            inc(&c.errors);
            eprintln!("hk-pipeline: dmr-tier2 message stream: {e}");
            return;
        }
    };
    if let Some(sink) = &shared.cfg.stream_sink {
        sink(&header, publisher.handle());
    }
    for h in scan.headers() {
        let record = MessageRecord {
            t,
            emitter_id: Some(emitter),
            provenance_ref: None,
            content_class: ContentClass::MetadataOnly,
            decode_id: None,
            annotation_id: None,
            decoder: Some(DMR_TIER2_VERSION.into()),
            frame_model: Some(format!("dmr-tier2/{}", h.name())),
            // Every header here passed its own FEC or CRC check; that is the condition for being
            // in this loop at all.
            crc_status: Some(CrcStatus::Valid),
            identity: None,
            metadata: header_json(h),
            content: None,
        };
        match publisher.publish_message(&record) {
            Ok(_) => inc(&c.dmr_headers),
            Err(StreamError::ContentGated { .. }) => inc(&c.dmr_headers_gated),
            Err(e) => {
                inc(&c.errors);
                eprintln!("hk-pipeline: dmr-tier2 message: {e}");
            }
        }
    }
}
