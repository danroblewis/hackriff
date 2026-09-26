//! **Auto-selecting a demod + decode pipeline from what was measured, and saying why** (T-546;
//! ADR-0015, ADR-0021).
//!
//! This is the north-star workflow's fourth step made real for the one emission class that can
//! reach it today: detect blindly, characterise, auto-select the pipeline, **confirm by decoding**.
//!
//! # What T-545 measured, and what each part of this closes
//!
//! | Gap | What it was | What this does |
//! |---|---|---|
//! | (2) estimate | the confirmed digital channel had `estimated_params: null` while the analogue neighbours got sessions saying `mod_order: 2` | [`analysis`] carries a measured [`hk_demod::fsk::FmStructure`] and [`attach`] persists it as a Demodulation with `mod_order: 4` |
//! | (3) auto-select | `POST /api/analyze` answered `501`; nothing in the repo picked a chain for a digital emission | the [`hk_model::repo::synthesis::EmitterSynthesis`] row this writes **is** the answer, and `/api/analyze` serves it |
//! | (4) decode | the decode lived in `trunk_system`; the inventory emitter at the same frequency stayed an unexplained blob | [`attach`] writes the decode onto **that emitter** as decoder evidence and re-ranks it |
//! | (5b) explanation | the only suggestion scored `evidence_confidence: 0`, flagged `allocation-only` | the decoder evidence maps to `public-safety` at 0.97 in [`crate::family`], so the ranking carries a measurement |
//!
//! # The decision is inspectable, and that is the point
//!
//! "A black box that happens to decode BART is not the capability." Every row carries an ADR-0021
//! trace whose nodes say what was considered and **why each alternative left**, with `tried` and
//! not-tried kept structurally apart:
//!
//! - the level hypotheses *were tried* and the loser carries its measurement — "two-level fits
//!   badly: 48 % of symbols sit on the inner pair, where a two-level alphabet puts none";
//! - the **linear** families (PSK/QAM) were *not* tried, with outcome `unsupported`: `psk_demod`
//!   exists since T-609, but this S1 search does not yet run a linear-modulation hypothesis
//!   through it (and QAM has no block at all). "We did not look" and "we looked and it scored
//!   badly" are different answers and are never rendered alike;
//! - every framing in the catalogue *was tried* and the two that lost carry their own sync-hit and
//!   CRC counts, so "why P25 and not DMR" is answered from measurements rather than asserted.
//!
//! # Blind
//!
//! Nothing here reads a band plan, a licence table or a frequency. It is handed measurements —
//! occupancy, a level count, a symbol rate, sync hits, CRC counts — and a frequency only as the
//! address of the emitter to write to. The known-signal database still explains afterwards, ranked
//! *below* this evidence rather than instead of it (ADR-0017).

pub mod acquire; // T-857 (MAUTO M-6): ring read, burst set, pin-on-analyze
pub mod attach; // T-860 (MAUTO M-9): attach, synthesized decodes, ConfirmPolicy.synthesized
pub mod jobs; // T-859 (MAUTO M-8): /api/analyze jobs, stream, cancel

use hk_demod::fsk::{FmStructure, Levels};
use hk_detect::trunk::{
    CcEvidence, CcFraming, ConfirmedCc, MIN_CC_FCO, MIN_CRC_VALID, MIN_SYNC_HITS,
};
use hk_model::cluster::{Fingerprint, MeasurementKey, Sighting};
use hk_model::repo::synthesis::{
    EmitterSynthesis, Measured, Outcome, ReceiverFit, Resolution, ResolutionKind, ResolutionReason,
    SYNTHESIZED_BY_OUTPUT_ANALYSIS, Stage, StageEvidence, SynthPipeline, TraceNode, Verdict,
};
use hk_model::{
    Classification, Demodulation, DemodulationId, EmitterId, EstimatedParams, LinkTarget,
    RepoError, Repository, TimeRange, Timestamp, TrunkProtocol,
};

/// Engine id and version recorded on every row this writes.
pub const TRUNK_SYNTH_ENGINE: &str = "hk-pipeline/trunk-synth@0.1.0";

/// The `Decoder` evidence id for each control-channel framing (the [`crate::family`] vocabulary).
pub fn decoder_id(framing: CcFraming) -> &'static str {
    match framing {
        CcFraming::P25Phase1 => "p25-tsbk",
        CcFraming::DmrBsData => "dmr-csbk",
        CcFraming::NxdnCac => "nxdn-cac",
    }
}

/// What one framing scored when it was tried, for the trace's "why not that one".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FramingScore {
    /// The framing.
    pub framing: CcFraming,
    /// Sync-pattern hits.
    pub sync_hits: u32,
    /// CRC-valid blocks.
    pub crc_valid: u32,
    /// Blocks whose CRC was checked.
    pub crc_checked: u32,
}

/// Everything one confirmed control channel was measured to be.
///
/// It is deliberately all **measurements plus one address**: no band, no allocation, no expected
/// protocol.
#[derive(Clone, Debug)]
pub struct CcObservation<'a> {
    /// Channel centre, RF Hz — where the emission was found, not where it was looked for.
    pub center_hz: f64,
    /// Channel filter bandwidth used, Hz.
    pub bandwidth_hz: f64,
    /// Frequency-channel occupancy, 0–1.
    pub fco: f64,
    /// Blind symbol-structure measurement, `None` when it refused to claim one.
    pub structure: Option<FmStructure>,
    /// Every framing the catalogue offers, with what it scored.
    pub framings: &'a [FramingScore],
    /// The framing that confirmed.
    pub confirmed: &'a ConfirmedCc,
    /// The protocol the decoded messages named, if any.
    pub protocol: TrunkProtocol,
    /// What the receiver itself contributed.
    pub receiver: Option<ReceiverFit>,
    /// Time of the IQ analysed.
    pub t: Timestamp,
}

impl CcObservation<'_> {
    /// The evidence of the confirming framing.
    fn evidence(&self) -> &CcEvidence {
        self.confirmed.evidence()
    }
}

/// The measured parameters to persist, or `None` when the measurement abstained.
///
/// **Abstention survives into the data model or it was never made.** `docs/api.md` is explicit
/// that `estimated_params` holds measured values only, never a fabricated default, so an emission
/// whose level structure could not be measured gets no session rather than a plausible one.
pub fn estimated_params(obs: &CcObservation<'_>) -> Option<(&'static str, EstimatedParams)> {
    let s = obs.structure?;
    let label = s.levels.label()?;
    Some((
        label,
        EstimatedParams {
            symbol_rate_hz: Some(s.symbol_rate_bd),
            deviation_hz: Some(s.outer_deviation_hz),
            cfo_hz: Some(s.residual_cfo_hz),
            mod_order: s.levels.order(),
            roll_off: None,
            bandwidth_hz: Some(obs.bandwidth_hz),
            pilot_hz: None,
            subaudible: None,
        },
    ))
}

/// Builds the analysis row for `emitter` from `obs`.
///
/// Pure: every number in the result is one that was measured, and the wording is rendered here so
/// no client does it (ADR-0015 §3.4).
pub fn analysis(emitter: EmitterId, obs: &CcObservation<'_>) -> EmitterSynthesis {
    let ev = obs.evidence();
    let confirming = obs.confirmed.framing();
    let mut trace = Vec::new();
    let mut evidence = Vec::new();

    // ---- S0. The channel: continuously occupied, which is what made it worth demodulating.
    let fco_bits = occupancy_bits(obs.fco);
    evidence.push(StageEvidence {
        stage: Stage::S0Channel,
        metric: "occupancy".into(),
        raw: obs.fco,
        n: 1,
        bits: fco_bits,
        summary: format!(
            "{:.0} % frequency-channel occupancy over a {:.1} kHz channel at {:.4} MHz \
             (candidacy floor {:.0} %)",
            obs.fco * 100.0,
            obs.bandwidth_hz / 1e3,
            obs.center_hz / 1e6,
            MIN_CC_FCO * 100.0,
        ),
    });
    trace.push(
        TraceNode::at("n0", Stage::S0Channel, "channel", "narrowband-continuous")
            .seed("estimate")
            .evaluations(1)
            .tried(
                Outcome::Survived,
                Measured {
                    metric: "occupancy".into(),
                    raw: obs.fco,
                    n: 1,
                    bits: fco_bits,
                },
                format!(
                    "continuously occupied at {:.0} %: worth demodulating, and on its own worth \
                     nothing more — a continuous data emitter looks exactly like this",
                    obs.fco * 100.0,
                ),
            ),
    );

    // ---- S1. How many levels, and the alternatives.
    let s1 = level_nodes(obs);
    trace.extend(s1);
    if let Some(s) = obs.structure {
        evidence.push(StageEvidence {
            stage: Stage::S1Demod,
            metric: "bimodality".into(),
            raw: s.inner_fraction,
            n: s.symbols as u64,
            bits: level_bits(&s),
            summary: format!(
                "{} discrete levels: {:.0} % of {} symbols sit on the inner pair and the mean \
                 distance to a level is {:.3} of the outer deviation ({:.0} Hz)",
                s.levels.order().map_or("no".into(), |o| o.to_string()),
                s.inner_fraction * 100.0,
                s.symbols,
                s.level_fit,
                s.outer_deviation_hz,
            ),
        });
        // ---- S2. The clock line.
        evidence.push(StageEvidence {
            stage: Stage::S2Clock,
            metric: "eye_open".into(),
            raw: s.symbol_rate_bd,
            n: s.symbols as u64,
            bits: s.clock_bits,
            summary: format!(
                "symbol clock at {:.1} Bd, {:.1} bits above the band median — measured from the \
                 cyclostationary line, not configured",
                s.symbol_rate_bd, s.clock_bits,
            ),
        });
        trace.push(
            TraceNode::at("n2", Stage::S2Clock, "clock", "cyclostationary-line")
                .seed("estimate")
                .child_of("n1")
                .evaluations(1)
                .tried(
                    Outcome::Survived,
                    Measured {
                        metric: "eye_open".into(),
                        raw: s.symbol_rate_bd,
                        n: s.symbols as u64,
                        bits: s.clock_bits,
                    },
                    format!("symbol rate {:.1} Bd", s.symbol_rate_bd),
                ),
        );
    }

    // ---- S4/S5. Framing and check: every framing in the catalogue, and what each scored.
    for (i, f) in obs.framings.iter().enumerate() {
        let won = f.framing == confirming;
        let bits = sync_bits(f.sync_hits);
        trace.push(
            TraceNode::at(
                format!("n4_{i}"),
                Stage::S4Framing,
                framing_family(f.framing),
                decoder_id(f.framing),
            )
            .seed("template")
            .child_of("n1")
            .evaluations(1)
            .tried(
                if won {
                    Outcome::Survived
                } else {
                    Outcome::PrunedFloor
                },
                Measured {
                    metric: "sync_excess".into(),
                    raw: f64::from(f.sync_hits),
                    n: u64::from(f.crc_checked),
                    bits,
                },
                if won {
                    format!(
                        "{}: {} frame syncs and {} of {} blocks CRC-valid — sync AND check, which \
                         is what confirmation requires",
                        f.framing.name(),
                        f.sync_hits,
                        f.crc_valid,
                        f.crc_checked,
                    )
                } else {
                    format!(
                        "{}: {} frame syncs, {} of {} blocks CRC-valid — below the floor of {} \
                         syncs and {} valid blocks, so this air interface is not what is \
                         transmitting",
                        f.framing.name(),
                        f.sync_hits,
                        f.crc_valid,
                        f.crc_checked,
                        MIN_SYNC_HITS,
                        MIN_CRC_VALID,
                    )
                },
            ),
        );
    }
    evidence.push(StageEvidence {
        stage: Stage::S4Framing,
        metric: "sync_excess".into(),
        raw: f64::from(ev.sync_hits()),
        n: u64::from(ev.crc_checked()),
        bits: sync_bits(ev.sync_hits()),
        summary: format!(
            "{} {} frame syncs at the expected spacing",
            ev.sync_hits(),
            confirming.name(),
        ),
    });
    evidence.push(StageEvidence {
        stage: Stage::S5Check,
        metric: "check_distinct_valid".into(),
        raw: f64::from(ev.crc_valid()),
        n: u64::from(ev.crc_checked()),
        bits: check_bits(ev.crc_valid()),
        summary: format!(
            "{} of {} blocks CRC-valid: the check is what confirms, and it is what makes this a \
             decode rather than a resemblance",
            ev.crc_valid(),
            ev.crc_checked(),
        ),
    });

    // ---- The choice, and what the absence of a fuller one means.
    //
    // Two things can hold it back independently: the messages may not have named a system, and
    // the modulation alphabet may not have been measurable. They are different absences, so they
    // get different sentences rather than one shrug.
    let named = obs.protocol != TrunkProtocol::Unknown;
    let alphabet = obs.structure.and_then(|s| s.levels.order());
    let verdict = match (named, alphabet.is_some()) {
        (true, true) => Verdict::Solved,
        (true, false) | (false, false) => Verdict::Framed,
        (false, true) => Verdict::Checked,
    };
    let resolution = (verdict != Verdict::Solved).then(|| {
        let missing = match (named, alphabet.is_some()) {
            (false, true) => "no corroborated trunking message named a system, so the air \
                              interface is known and the system is not"
                .to_owned(),
            (true, false) => format!(
                "the modulation alphabet could not be measured, so no demodulator could be bound \
                 even though {} named the system",
                confirming.name(),
            ),
            _ => "neither the modulation alphabet nor a system name could be established from \
                  this window"
                .to_owned(),
        };
        Resolution {
            kind: ResolutionKind::StructuredUnidentified,
            deepest_verdict: Some(verdict),
            reason: Some(ResolutionReason::NothingScored),
            suspected: None,
            summary: format!(
                "framed and check-valid under {} — {} of {} blocks pass — but {missing}. That is \
                 a result, not a failure: a confirmed emitter with no complete identification is \
                 a legal state (ADR-0021 §7A.5).",
                confirming.name(),
                ev.crc_valid(),
                ev.crc_checked(),
            ),
        }
    });
    let pipeline = obs.structure.and_then(|s| {
        let demod = s.levels.label()?;
        Some(SynthPipeline {
            demod: demod.into(),
            decode: Some(decoder_id(confirming).into()),
            params: vec![
                ("symbol_rate_bd".into(), s.symbol_rate_bd),
                ("outer_deviation_hz".into(), s.outer_deviation_hz),
                ("bandwidth_hz".into(), obs.bandwidth_hz),
                ("residual_cfo_hz".into(), s.residual_cfo_hz),
            ],
            summary: format!(
                "{demod} at {:.0} Bd into {}: chosen because the emission measured {} discrete \
                 levels at ±{:.0} Hz on a {:.1} kHz continuous channel, and of the {} framings \
                 tried only {} produced frame sync with CRC-valid blocks ({} of {})",
                s.symbol_rate_bd,
                decoder_id(confirming),
                s.levels.order().unwrap_or(0),
                s.outer_deviation_hz,
                obs.bandwidth_hz / 1e3,
                obs.framings.len(),
                confirming.name(),
                ev.crc_valid(),
                ev.crc_checked(),
            ),
        })
    });

    EmitterSynthesis {
        emitter_id: emitter,
        provenance: SYNTHESIZED_BY_OUTPUT_ANALYSIS.into(),
        engine: TRUNK_SYNTH_ENGINE.into(),
        t: obs.t,
        verdict,
        stage_reached: Stage::S5Check,
        pipeline,
        evidence,
        trace,
        resolution,
        receiver: obs.receiver,
        job: None,
    }
}

/// The S1 trace nodes.
///
/// Both level hypotheses were **tried** — one measurement decides both — so both carry what they
/// measured. The linear families were **not**, and say so with `unsupported`: ADR-0015 §1.1's
/// catalogue gap is a real reason a hypothesis never ran, and it must not read like a hypothesis
/// that ran and lost.
fn level_nodes(obs: &CcObservation<'_>) -> Vec<TraceNode> {
    let unsupported = TraceNode::at("n1_psk", Stage::S1Demod, "psk", "linear-modulation")
        .child_of("n0")
        .not_tried(
            Outcome::Unsupported,
            "this search does not yet run a linear-modulation hypothesis (psk_demod exists since \
             T-609, ADR-0011 §9, but S1 does not try it, and QAM has no block), so a linear \
             modulation was never looked at here. That is not the same as looking and finding \
             nothing.",
        );
    let Some(s) = obs.structure else {
        return vec![
            TraceNode::at("n1", Stage::S1Demod, "fm", "discrete-level-fm")
                .seed("estimate")
                .child_of("n0")
                .evaluations(1)
                .tried(
                    Outcome::PrunedFloor,
                    Measured {
                        metric: "bimodality".into(),
                        raw: 0.0,
                        n: 0,
                        bits: 0.0,
                    },
                    "no symbol alphabet could be measured: the emission framed and checked, but \
                     what it is modulated with was not established",
                ),
            unsupported,
        ];
    };
    let winner = s.levels;
    let node = |id: &str, choice: &str, order: u32, summary: String| {
        TraceNode::at(id, Stage::S1Demod, "fm", choice)
            .seed("estimate")
            .child_of("n0")
            .evaluations(1)
            .tried(
                if winner.order() == Some(order) {
                    Outcome::Survived
                } else {
                    Outcome::PrunedFloor
                },
                Measured {
                    metric: "bimodality".into(),
                    raw: s.inner_fraction,
                    n: s.symbols as u64,
                    bits: level_bits(&s),
                },
                summary,
            )
    };
    let four = node(
        if winner == Levels::Four { "n1" } else { "n1_4" },
        "four-level-fm",
        4,
        format!(
            "four-level FM: {:.0} % of symbols on the inner pair (a balanced four-level alphabet \
             puts ~50 % there), mean distance to a level {:.3}",
            s.inner_fraction * 100.0,
            s.level_fit,
        ),
    );
    let two = node(
        if winner == Levels::Two { "n1" } else { "n1_2" },
        "two-level-fm",
        2,
        format!(
            "two-level FM: {:.0} % of symbols on the inner pair, where a two-level alphabet puts \
             none",
            s.inner_fraction * 100.0,
        ),
    );
    vec![four, two, unsupported]
}

/// The hk-mod family a control-channel framing belongs to, for "why not that one" filtering.
fn framing_family(f: CcFraming) -> &'static str {
    match f {
        CcFraming::P25Phase1 => "p25",
        CcFraming::DmrBsData => "dmr",
        CcFraming::NxdnCac => "nxdn",
    }
}

/// Occupancy as bits over the candidacy floor. A channel at the floor scores 0; a 100 %-duty one
/// scores the log-odds of that duty arising by chance from a bursty band.
fn occupancy_bits(fco: f64) -> f64 {
    let f = fco.clamp(0.0, 1.0);
    if f <= MIN_CC_FCO {
        0.0
    } else {
        -((1.0 - f).max(1e-6)).log2() - -((1.0 - MIN_CC_FCO).max(1e-6)).log2()
    }
}

/// Level evidence in bits: how far the inner-pair population is from the *other* hypothesis,
/// over the symbols that measured it. A binomial tail, so it grows with support rather than with
/// confidence alone.
fn level_bits(s: &FmStructure) -> f64 {
    let n = s.symbols as f64;
    if n <= 0.0 {
        return 0.0;
    }
    let d = match s.levels {
        Levels::Four => (s.inner_fraction - 0.0).abs(),
        Levels::Two => (0.5 - s.inner_fraction).abs(),
        Levels::Indeterminate => return 0.0,
    };
    // 2 n d² / ln 2 is the Chernoff exponent for a binomial deviation of d over n trials.
    (2.0 * n * d * d) / std::f64::consts::LN_2
}

/// Frame-sync excess in bits. `hk_detect::trunk` measures the per-frame sync false-alarm rate as
/// ~2⁻²⁴ for the patterns in its catalogue, so each independent hit is worth that much.
fn sync_bits(hits: u32) -> f64 {
    f64::from(hits) * 24.0
}

/// Check evidence in bits: the block CRC is 16 bits wide, so each distinct valid block is 16.
fn check_bits(valid: u32) -> f64 {
    f64::from(valid) * 16.0
}

/// What [`attach`] wrote.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Attached {
    /// The emitter the decode was filed against.
    pub emitter: EmitterId,
    /// Whether that emitter had to be created, because blind detection had not yet offered a row
    /// at this frequency.
    pub created: bool,
    /// The demodulation session.
    pub demodulation: DemodulationId,
    /// Whether decoder evidence was recorded on the emitter.
    pub classified: bool,
}

/// Files a confirmed control channel against the inventory: the emitter, the measured parameters,
/// the analysis, and the decode as **evidence on the emitter itself**.
///
/// That last part is the product vision's step 4 in the user's own words — *"successful decode
/// confirms it"*. Before T-546 the decode named a system in a `trunk_system` row while the emitter
/// at the same frequency read `family: null`, `estimated_params: null`, and the two `docs/07`
/// object graphs never met.
///
/// **Which emitter.** The one blind detection already has at this channel
/// ([`crate::refine::emitter_for_channel`]) when there is one. When there is not — and there
/// routinely is not, because a control channel confirms within the first window while the
/// detector is still accumulating a track — the Sighting is **recorded**, exactly as the analogue
/// and FSK chain writers record theirs, and the repository's clustering resolves it to the
/// detector's row as soon as one appears. This is not the inventory conjuring a signal: a
/// continuously occupied narrowband channel whose frame sync *and* CRC both hold is the strongest
/// evidence in the whole run, and the chain that measured it is a detector.
///
/// The caller must run `Inventory::chain_emitter` afterwards to re-rank the explanations, exactly
/// as the plugin-decoder path does (`crate::family::record_decoder_evidence`'s call contract).
pub fn attach(
    repo: &mut Repository,
    emitter: Option<EmitterId>,
    obs: &CcObservation<'_>,
) -> Result<Attached, RepoError> {
    let measured = estimated_params(obs);
    // The demodulation is real whether or not the blind structure measurement committed: the
    // four-level demodulator ran and its dibits CRC-checked. What is conditional is the
    // *parameters* — `docs/api.md` is explicit that `estimated_params` holds measured values
    // only, so an abstaining measurement leaves them empty rather than plausible.
    let demod_id = DemodulationId::new();
    let (mode, params) = match measured {
        Some((m, p)) => (m, p),
        None => (DEMODULATED_AS, EstimatedParams::default()),
    };

    let (emitter, created) = match emitter {
        Some(e) => (repo.live_emitter_id(e)?, false),
        None => {
            let sighting = Sighting {
                source: LinkTarget::Demodulation(demod_id),
                seen: TimeRange::new(obs.t, obs.t),
                count: 1,
                f_center_hz: obs.center_hz,
                bandwidth_hz: obs.bandwidth_hz,
                fingerprint: Some(Fingerprint {
                    family: Some(mode.to_owned()),
                    symbol_rate_hz: params.symbol_rate_hz,
                    deviation_hz: params.deviation_hz,
                    duty_cycle: Some(obs.fco),
                    ..Fingerprint::new(obs.center_hz, obs.bandwidth_hz)
                }),
                identity: None,
                context: None,
                classification: classification(obs, obs.t),
                tags: Vec::new(),
            };
            let r = repo.record_sighting_measured(
                &sighting,
                &MeasurementKey::new(TRUNK_SYNTH_ENGINE),
                None,
            )?;
            (r.emitter_id, true)
        }
    };

    repo.insert_demodulation(&Demodulation {
        id: demod_id,
        emitter_ref: Some(emitter),
        detection_ref: None,
        recording_ref: None,
        mode: mode.into(),
        params,
        lock_quality: obs.structure.map(|s| (1.0 - s.level_fit).clamp(0.0, 1.0)),
        evm_db: None,
        time: TimeRange::new(obs.t, obs.t),
        demod_version: hk_demod::fsk::STRUCTURE_VERSION.into(),
    })?;
    repo.insert_synthesis(&analysis(emitter, obs))?;

    // The decode as family evidence. A CRC-valid trunked control channel is not a probabilistic
    // call, so the confidence is the vocabulary's own.
    let id = decoder_id(obs.confirmed.framing());
    let classified =
        crate::family::record_decoder_evidence(repo, emitter, id, 1.0, obs.t)?.is_some();
    Ok(Attached {
        emitter,
        created,
        demodulation: demod_id,
        classified,
    })
}

/// The `mode` of a session whose blind structure measurement abstained: **what actually ran**.
///
/// Not a claim about the alphabet — that claim lives in `estimated_params`, which stays empty —
/// but the four-level demodulator did run and its dibits did CRC-check, and naming the
/// demodulator that produced them is a fact rather than an inference.
const DEMODULATED_AS: &str = "c4fm";

/// The Classification a confirmed control channel warrants, for callers that want it without the
/// repository write.
pub fn classification(obs: &CcObservation<'_>, t: Timestamp) -> Option<Classification> {
    crate::family::decoder_evidence(decoder_id(obs.confirmed.framing()), 1.0, t)
}

#[cfg(test)]
mod tests {
    use hk_demod::fsk::Levels;
    use hk_detect::trunk::{CcCandidate, RASTER_TOLERANCE_HZ, best_lmr_raster};

    use super::*;

    const CENTRE: f64 = 851_050_000.0;

    fn structure(levels: Levels, inner: f64) -> FmStructure {
        FmStructure {
            symbol_rate_bd: 4800.4,
            clock_bits: 31.0,
            levels,
            inner_fraction: inner,
            outer_deviation_hz: 1806.0,
            residual_cfo_hz: -12.0,
            level_fit: 0.03,
            valley_ratio: 0.04,
            symbols: 4096,
            timing_phase: 1.2,
        }
    }

    fn framings(winner: CcFraming) -> Vec<FramingScore> {
        hk_detect::trunk::CC_FRAMINGS
            .iter()
            .map(|&f| FramingScore {
                framing: f,
                sync_hits: if f == winner { 42 } else { 0 },
                crc_valid: if f == winner { 37 } else { 0 },
                crc_checked: if f == winner { 40 } else { 41 },
            })
            .collect()
    }

    /// A `ConfirmedCc` is unconstructible without evidence by design, so this builds real framed
    /// blocks and puts them through the real confirmer — the same shape
    /// `hk_detect::trunk::confirm`'s own tests use.
    fn confirmed() -> ConfirmedCc {
        fn crc16(data: &[u8]) -> u16 {
            let mut crc = 0xFFFFu16;
            for &b in data {
                crc ^= u16::from(b) << 8;
                for _ in 0..8 {
                    crc = if crc & 0x8000 != 0 {
                        (crc << 1) ^ 0x1021
                    } else {
                        crc << 1
                    };
                }
            }
            crc
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut byte = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (state >> 33) as u8
        };
        let mut dibits = Vec::new();
        for _ in 0..8 {
            dibits.extend_from_slice(&hk_detect::trunk::P25_FRAME_SYNC_DIBITS);
            let data: Vec<u8> = (0..10).map(|_| byte()).collect();
            let mut block = data.clone();
            block.extend_from_slice(&crc16(&data).to_be_bytes());
            dibits.extend(
                block
                    .iter()
                    .flat_map(|&b| [(b >> 6) & 3, (b >> 4) & 3, (b >> 2) & 3, b & 3]),
            );
        }
        let fit = best_lmr_raster(CENTRE, CENTRE, RASTER_TOLERANCE_HZ).unwrap();
        let cand = CcCandidate::new(CENTRE, 12_500.0, 1.0, fit).expect("continuous and on-raster");
        hk_detect::trunk::CcConfirmer::default()
            .confirm_any(&cand, &dibits)
            .expect("sync and CRC both present")
    }

    fn obs<'a>(
        s: Option<FmStructure>,
        f: &'a [FramingScore],
        c: &'a ConfirmedCc,
        protocol: TrunkProtocol,
    ) -> CcObservation<'a> {
        CcObservation {
            center_hz: CENTRE,
            bandwidth_hz: 12_500.0,
            fco: 1.0,
            structure: s,
            framings: f,
            confirmed: c,
            protocol,
            receiver: None,
            t: Timestamp::UNIX_EPOCH,
        }
    }

    /// **The ticket's headline assertion, at the unit level**: the pipeline is named from the
    /// measurements, and the answer says why in terms of them.
    #[test]
    fn the_pipeline_is_chosen_from_the_measurements_and_says_so() {
        let c = confirmed();
        let f = framings(c.framing());
        let o = obs(
            Some(structure(Levels::Four, 0.49)),
            &f,
            &c,
            TrunkProtocol::P25Phase1,
        );
        let row = analysis(EmitterId::new(), &o);
        row.validate().expect("a well-formed analysis");

        let p = row.pipeline.as_ref().expect("a pipeline was chosen");
        assert_eq!(p.demod, "c4fm", "four-level FM at a symbol clock is C4FM");
        assert_eq!(p.decode.as_deref(), Some(decoder_id(c.framing())));
        assert!(
            p.summary.contains("4800") && p.summary.contains("levels"),
            "the choice must be justified by the measurements: {}",
            p.summary,
        );
        assert!(
            p.params
                .iter()
                .any(|(k, v)| k == "symbol_rate_bd" && *v > 4700.0),
            "the bound parameters are the measured ones: {:?}",
            p.params,
        );
        assert_eq!(row.verdict, Verdict::Solved);
        assert!(
            row.resolution.is_none(),
            "a solved analysis needs no excuse"
        );
        assert!(
            row.evidence
                .iter()
                .any(|e| e.stage == Stage::S5Check && e.raw > 0.0),
            "the check is the evidence that confirms",
        );
    }

    /// **"Why not PSK" and "why not DMR" are different answers, and both must be in the trace.**
    /// One was never looked at (no block exists); the others were tried and measured badly.
    #[test]
    fn the_trace_separates_what_was_tried_from_what_was_never_looked_at() {
        let c = confirmed();
        let f = framings(c.framing());
        let o = obs(
            Some(structure(Levels::Four, 0.49)),
            &f,
            &c,
            TrunkProtocol::P25Phase1,
        );
        let row = analysis(EmitterId::new(), &o);

        let psk = row
            .trace
            .iter()
            .find(|n| n.family == "psk")
            .expect("the linear families must appear, or the search looks exhaustive");
        assert_eq!(psk.outcome, Outcome::Unsupported);
        assert!(!psk.tried, "nothing was measured about PSK");
        assert!(psk.measured.is_none());

        let two = row
            .trace
            .iter()
            .find(|n| n.choice == "two-level-fm")
            .expect("the losing level hypothesis must carry its measurement");
        assert_eq!(two.outcome, Outcome::PrunedFloor);
        assert!(two.tried && two.measured.is_some());

        // Every framing in the catalogue is accounted for, winner and losers alike.
        let framed: Vec<_> = row
            .trace
            .iter()
            .filter(|n| n.stage == Stage::S4Framing)
            .collect();
        assert_eq!(framed.len(), hk_detect::trunk::CC_FRAMINGS.len());
        assert_eq!(
            framed
                .iter()
                .filter(|n| n.outcome == Outcome::Survived)
                .count(),
            1,
            "exactly one framing confirmed",
        );
        assert!(
            framed.iter().all(|n| n.tried && n.measured.is_some()),
            "every framing was actually run: each carries its sync/CRC counts",
        );
    }

    /// A confirmed channel whose messages name no system is **structured and unidentified** — a
    /// real result with its own resolution, not a silent gap (ADR-0021 §7A.5).
    #[test]
    fn a_confirmed_but_unnamed_system_resolves_structured_unidentified() {
        let c = confirmed();
        let f = framings(c.framing());
        let o = obs(
            Some(structure(Levels::Four, 0.49)),
            &f,
            &c,
            TrunkProtocol::Unknown,
        );
        let row = analysis(EmitterId::new(), &o);
        row.validate().unwrap();
        let r = row
            .resolution
            .expect("an unsolved analysis says what the absence means");
        assert_eq!(r.kind, ResolutionKind::StructuredUnidentified);
        assert_eq!(row.verdict, Verdict::Checked);
    }

    /// An emission whose alphabet could not be measured gets **no** parameters, rather than
    /// plausible ones — and the verdict is capped at `framed` to match.
    #[test]
    fn an_unmeasurable_alphabet_yields_no_estimated_params_and_a_capped_verdict() {
        let c = confirmed();
        let f = framings(c.framing());
        let o = obs(None, &f, &c, TrunkProtocol::P25Phase1);
        assert!(estimated_params(&o).is_none());
        let row = analysis(EmitterId::new(), &o);
        row.validate().unwrap();
        assert!(row.pipeline.is_none(), "nothing to bind a demodulator with");
        assert_eq!(row.verdict, Verdict::Framed);

        // Same for a measurement that abstained on the level count.
        let o = obs(
            Some(structure(Levels::Indeterminate, 0.35)),
            &f,
            &c,
            TrunkProtocol::P25Phase1,
        );
        assert!(estimated_params(&o).is_none());
        assert_eq!(analysis(EmitterId::new(), &o).verdict, Verdict::Framed);
    }

    /// The measured parameters are the measured ones, and `mod_order` can say 4 — the answer
    /// T-545 found the repository could not produce at all.
    #[test]
    fn the_estimated_parameters_are_the_measured_ones_and_mod_order_can_say_four() {
        let c = confirmed();
        let f = framings(c.framing());
        let o = obs(
            Some(structure(Levels::Four, 0.49)),
            &f,
            &c,
            TrunkProtocol::P25Phase1,
        );
        let (mode, p) = estimated_params(&o).expect("a measured alphabet");
        assert_eq!(mode, "c4fm");
        assert_eq!(p.mod_order, Some(4));
        assert_eq!(p.symbol_rate_hz, Some(4800.4));
        assert_eq!(p.deviation_hz, Some(1806.0));
    }
}
