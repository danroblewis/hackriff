//! The `audio` output kind (ADR-0011 §8.2, T-866 = ADR-0015 LP-2): a recipe's `audio_out` sink
//! served as the stream contract §12.2 **audio profile, unchanged** — `kind: audio`, `ri16_le`,
//! 48 kS/s, 960-sample data records, type-3 status records — on `audio/<pipeline>/<output>`.
//!
//! - **Header:** the Listen keys keep their names and meanings; what a hand-started recipe did
//!   not measure is said so (`mode_rules: "recipe-declared"`, `mode_confidence: 0`, no `snr_db`)
//!   rather than invented. Additive: `pipeline_id`, `recipe` (`<id>@<version>`), `output_id`,
//!   `edit_rev`.
//! - **Status records:** one record per ~250 ms tick carries the §12.2 audio keys (what a dock
//!   meter reads) **and** the §1.3 `<node>.<metric>` batch (what the workbench reads).
//! - **Gating:** audio is content. The pipeline start, and an edit that adds audio, run Listen's
//!   pre-attach gate on the channel before any ring read ([`gate`]); egress gating is the
//!   publisher's, and a gated audio record stops the pipeline (fail closed), as it stops Listen.

use std::time::{Duration, Instant};

use hk_blocks::{AudioFrames, Status};
use hk_model::{EstimatedParams, Timestamp};
use hk_recipe::{AUDIO_OUT_BLOCK, OutputSpec, Params, Recipe};
use hk_stream::audio::{
    AUDIO_DATATYPE, AUDIO_FRAME_SAMPLES, AUDIO_MAX_FRAME_LEN, AUDIO_SAMPLE_RATE_HZ, AgcInfo,
    AudioInfo, AudioStatus, SquelchInfo,
};
use hk_stream::{
    BinaryRecord, OpenRefusal, Publisher, PublisherConfig, PublisherHandle, RecordFlags,
    StreamError, StreamHeader, StreamKind,
};
use serde_json::{Map, Value};

use crate::chains::listen::listen_class;
use crate::recipes::graph::Graph;
use crate::recipes::taps::StreamCtx;
use crate::run::Shared;

/// `audio.mode_rules` of a recipe's audio: the mode is the recipe's declaration, not an estimate.
pub const RECIPE_MODE_RULES: &str = "recipe-declared";

/// Per-consumer queue of an audio output, bytes: ≈ 0.6 s of 20 ms records, as Listen's.
const AUDIO_QUEUE_BYTES: usize = 64 * 1024;

/// Listen's pre-attach gate (`listen_class`) on the channel `[lo, hi]`: a recipe with an `audio`
/// output is refused here, before its channel DDC exists (ADR-0011 §8.3).
pub(crate) fn gate(shared: &Shared, lo: f64, hi: f64) -> Result<(), OpenRefusal> {
    listen_class(
        shared.cfg.source_class,
        &shared.cfg.settings.classify,
        lo,
        hi,
    )
    .map(|_| ())
}

fn node_params<'a>(recipe: &'a Recipe, block: &str) -> Option<&'a Params> {
    recipe
        .nodes
        .iter()
        .find(|n| n.block == block)
        .map(|n| &n.params)
}

fn num(p: Option<&Params>, key: &str) -> Option<f64> {
    p.and_then(|p| p.get(key)).and_then(Value::as_f64)
}

/// The header of `audio` output `spec` (ADR-0011 §8.2): the §12.2 profile, with the squelch,
/// AGC and de-emphasis settings the recipe's nodes run with.
pub fn audio_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    spec: &OutputSpec,
    edit_rev: u32,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        format!("audio/{}/{}", ctx.pipeline_id, spec.id),
        StreamKind::Audio,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.datatype = Some(AUDIO_DATATYPE.into());
    h.sample_rate_hz = Some(AUDIO_SAMPLE_RATE_HZ);
    h.center_hz = Some(ctx.center_hz);
    h.bandwidth_hz = Some(ctx.bandwidth_hz);
    h.emitter_id = ctx.emitter_id;
    h.max_frame_len = AUDIO_MAX_FRAME_LEN;
    let sq = node_params(recipe, "squelch");
    let agc = node_params(recipe, "agc");
    let profile = spec.profile.clone().unwrap_or_default();
    let deemphasis_s = profile
        .deemphasis_s
        .or_else(|| num(node_params(recipe, "deemphasis"), "tau_s"))
        .or_else(|| num(node_params(recipe, "fm_demod"), "deemphasis_s"))
        .filter(|t| *t > 0.0);
    let recipe_ref = format!("{}@{}", recipe.id, recipe.version);
    h.audio = Some(AudioInfo {
        channels: 1,
        frame_samples: AUDIO_FRAME_SAMPLES as u32,
        mode: profile.mode.unwrap_or_else(|| "unknown".into()),
        mode_confidence: 0.0,
        mode_rules: RECIPE_MODE_RULES.into(),
        params: EstimatedParams {
            bandwidth_hz: Some(ctx.bandwidth_hz),
            ..EstimatedParams::default()
        },
        snr_db: None,
        squelch: SquelchInfo {
            open_snr_db: num(sq, "open_snr_db").unwrap_or(if sq.is_some() { 6.0 } else { 0.0 }),
            hysteresis_db: num(sq, "hysteresis_db").unwrap_or(if sq.is_some() { 3.0 } else { 0.0 }),
            noise_dbfs: num(sq, "noise_dbfs"),
        },
        agc: AgcInfo {
            enabled: agc.is_some()
                && agc
                    .and_then(|p| p.get("enabled"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            target_dbfs: num(agc, "target_dbfs").unwrap_or(-6.0),
            max_gain_db: num(agc, "max_gain_db").unwrap_or(if agc.is_some() { 60.0 } else { 0.0 }),
        },
        deemphasis_s,
        demod: format!("recipe:{recipe_ref}"),
        refinement: None,
        pipeline_id: Some(ctx.pipeline_id.clone()),
        recipe: Some(recipe_ref),
        output_id: Some(spec.id.clone()),
        edit_rev: Some(edit_rev),
    });
    h
}

/// One `audio` output's publisher and counters.
pub struct AudioSink {
    publisher: Publisher,
    handle: PublisherHandle,
    frames: u64,
    /// `sample_index` just past the last published frame (the status records' index).
    next_index: u64,
    /// Added to the sink node's indices. A hot edit that rebuilds the `audio_out` node starts a
    /// fresh instance at 0 while the stream keeps its consumer, so the stream re-bases it: the
    /// wire's `sample_index` never goes backwards, and the re-based record is flagged.
    base: u64,
    latency_ms: f64,
}

impl AudioSink {
    /// A sink on a new audio publisher for `header`.
    pub fn new(header: StreamHeader) -> Result<Self, StreamError> {
        let publisher = Publisher::new(
            header,
            PublisherConfig {
                queue_bytes: AUDIO_QUEUE_BYTES,
                disconnect_after_drops: u64::MAX,
                disconnect_after: Duration::from_secs(5),
                max_consumers: 16,
                drain_timeout: Duration::from_millis(500),
            },
        )?;
        Ok(Self {
            handle: publisher.handle(),
            publisher,
            frames: 0,
            next_index: 0,
            base: 0,
            latency_ms: 0.0,
        })
    }

    /// The stream's handle.
    pub fn handle(&self) -> PublisherHandle {
        self.handle.clone()
    }

    /// The stream's header.
    pub fn header(&self) -> &StreamHeader {
        self.publisher.header()
    }

    /// Publishes the sink node's finished frames and clears them. `t_of` maps a source index to
    /// its time; `read_at` is when the chunk that completed them was read (the latency origin).
    /// An error is a gated record: the caller stops the pipeline (fail closed).
    pub fn publish(
        &mut self,
        frames: &mut AudioFrames,
        t_of: &dyn Fn(f64) -> Timestamp,
        read_at: Instant,
    ) -> Result<u64, StreamError> {
        let mut n = 0;
        let mut result = Ok(());
        for (f, payload) in frames.iter() {
            let mut index = f.sample_index + self.base;
            let mut discontinuity = f.discontinuity;
            if self.frames + n > 0 && index < self.next_index {
                self.base = self.next_index - f.sample_index;
                index = self.next_index;
                discontinuity = true;
            }
            let flags = if discontinuity {
                RecordFlags::DISCONTINUITY
            } else {
                RecordFlags::empty()
            };
            if let Err(e) = self.publisher.publish_binary(BinaryRecord {
                t: t_of(f.source_index),
                sample_index: index,
                flags,
                payload,
            }) {
                result = Err(e);
                break;
            }
            n += 1;
            self.next_index = index + AUDIO_FRAME_SAMPLES as u64;
        }
        frames.clear();
        if n > 0 {
            self.frames += n;
            self.latency_ms = read_at.elapsed().as_secs_f64() * 1e3;
        }
        result.map(|()| n)
    }

    /// Publishes one status record: the §12.2 audio keys from the graph's audio nodes plus the
    /// node batch `nodes` (ADR-0011 §8.2: one record, two vocabularies).
    pub fn status(
        &mut self,
        t: Timestamp,
        graph: &Graph,
        nodes: &Map<String, Value>,
        lost_samples: u64,
        backlog_s: f64,
        (refined, refine_updates): (Option<(f64, f64)>, u64),
    ) {
        let first = |block: &str| -> Option<Status> {
            graph
                .nodes
                .iter()
                .find(|n| n.block == block)
                .map(|n| n.instance.status())
        };
        let extra = |s: &Option<Status>, key: &str| {
            s.as_ref()
                .and_then(|s| s.extra.iter().find(|(k, _)| *k == key).map(|(_, v)| v))
        };
        let sq = first("squelch");
        let agc = first("agc");
        let out = first(AUDIO_OUT_BLOCK);
        let status = AudioStatus {
            level_dbfs: round2(extra(&out, "level_dbfs").unwrap_or(-200.0)),
            snr_db: sq
                .as_ref()
                .and_then(|s| s.snr_db)
                .map(|v| round2(f64::from(v))),
            squelch_open: extra(&sq, "open").is_none_or(|v| v > 0.5),
            agc_gain_db: round2(extra(&agc, "gain_db").unwrap_or(0.0)),
            frames: self.frames,
            squelched_frames: (extra(&sq, "squelched_s").unwrap_or(0.0) * AUDIO_SAMPLE_RATE_HZ
                / AUDIO_FRAME_SAMPLES as f64)
                .round() as u64,
            lost_samples,
            latency_ms: round2(self.latency_ms),
            backlog_s: round2(backlog_s),
            refined_center_hz: refined.map(|(c, _)| c),
            refined_bandwidth_hz: refined.map(|(_, b)| b),
            refine_updates,
            // A recipe's `audio` output is mono (hk-recipe; T-874 changed only Listen's opener).
            stereo: None,
            stereo_lock_losses: None,
        };
        let mut m = match status.to_value() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        for (k, v) in nodes {
            m.insert(k.clone(), v.clone());
        }
        let _ = self
            .publisher
            .publish_status(t, self.next_index, &Value::Object(m));
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
