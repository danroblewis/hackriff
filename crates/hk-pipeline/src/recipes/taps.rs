//! Stage-output taps and output publishers of recipe pipelines (ADR-0011 §1.3, §4; stream
//! contract §14; T-088).
//!
//! - **Frames** (`inspector` outputs, `frames` stage taps) go out through a [`FrameSink`]: one
//!   §14.2 `frame` record per frame ([`Publisher::publish_frame`]), plus one §14.3 `status`
//!   record per status tick and one `edit` record per applied hot edit
//!   ([`Publisher::publish_record`]). Their headers carry the §14.1 `inspector` profile.
//! - **Stage taps** ([`StageTap`]) publish an `iq`/`real`/`soft`/`bits` port's chunk as one binary
//!   data record (§14.4): `iq` as `cf32_le`, `real` as `rf32_le` (audio kind), `soft` as
//!   `rf32_le` (symbols), `bits` as `ru8`. A tap with no open consumer publishes nothing and
//!   marks the next record `DISCONTINUITY`; the runtime computes a port only while it is tapped
//!   when the block marks it diagnostic.
//!
//! Every publisher is drop-not-block (§7): nothing here can hold back the pipeline thread or the
//! ring.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hk_blocks::{ChunkFlags, FrameInfo, Output, PortVec};
use hk_model::{ContentClass, EmitterId, Timestamp};
use hk_recipe::{PortType, Recipe};
use hk_stream::inspector::{
    ChannelInfo, FRAME_RECORD_TYPE, FitStatus, FrameContent, FrameMetadata, FrameRecord,
    INSPECTOR_MESSAGE_SCHEMA, InspectorProfile, InspectorRecordType, InspectorSource, to_hex,
};
use hk_stream::{
    BinaryRecord, MetadataPolicy, MetadataType, Publisher, PublisherConfig, PublisherHandle,
    RecordFlags, StreamError, StreamHeader, StreamKind,
};
use serde_json::{Map, Value};

use crate::recipes::tap_eye::EyeTap;
use crate::recipes::tap_spectrum::{self, SpectrumTap};
use crate::recipes::tap_sync_search::SyncSearchTap;

/// Most frame bytes a frame record serialises (bigger frames are cut; `bit_len` stays true).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// What every frame of a pipeline output shares.
pub struct FrameCtx<'a> {
    /// `recipe:<id>@<version>`.
    pub decoder: &'a str,
    /// Frame model (the recipe id).
    pub frame_model: &'a str,
    /// Target emitter.
    pub emitter_id: Option<EmitterId>,
    /// Channel centre, Hz.
    pub channel_hz: f64,
    /// Follow-hops: channel centre by channel index (`FrameInfo::channel`), Hz; empty for a
    /// single-channel pipeline (every frame is at `channel_hz`).
    pub channels_hz: &'a [f64],
    /// Saved recipe version.
    pub recipe_version: u32,
    /// Live edit revision.
    pub edit_rev: u32,
}

/// Where a pipeline's frames go.
pub trait FrameSink: Send {
    /// Publishes one frame record.
    fn frame(
        &mut self,
        t: Timestamp,
        bytes: &[u8],
        info: &FrameInfo,
        ctx: &FrameCtx<'_>,
    ) -> Result<(), StreamError>;

    /// Publishes a metadata-only `status` or `edit` record (flat allowlist-shaped metadata).
    fn record(
        &mut self,
        t: Timestamp,
        record_type: InspectorRecordType,
        metadata: Map<String, Value>,
    ) -> Result<(), StreamError>;

    /// The stream's handle.
    fn handle(&self) -> PublisherHandle;

    /// The stream's header.
    fn header(&self) -> &StreamHeader;
}

/// §14 inspector records through a gated [`Publisher`].
pub struct InspectorSink {
    publisher: Publisher,
    handle: PublisherHandle,
    frames: u64,
}

impl InspectorSink {
    /// A sink on a new messages publisher for `header`; `policy` is required when the header's
    /// class forbids content ([`inspector_policy`]).
    pub fn new(
        header: StreamHeader,
        config: PublisherConfig,
        policy: Option<MetadataPolicy>,
    ) -> Result<Self, StreamError> {
        let publisher = match policy {
            Some(p) => Publisher::with_metadata_policy(header, config, p)?,
            None => Publisher::new(header, config)?,
        };
        Ok(Self {
            handle: publisher.handle(),
            publisher,
            frames: 0,
        })
    }
}

impl FrameSink for InspectorSink {
    fn frame(
        &mut self,
        t: Timestamp,
        bytes: &[u8],
        info: &FrameInfo,
        ctx: &FrameCtx<'_>,
    ) -> Result<(), StreamError> {
        let index = self.frames;
        self.frames += 1;
        let class = self.publisher.header().content_class;
        let rec = FrameRecord {
            record_type: FRAME_RECORD_TYPE.into(),
            seq: 0,
            t: t.as_unix_nanos(),
            content_class: class,
            gated: false,
            crc_status: Some(info.check),
            decoder: Some(ctx.decoder.to_owned()),
            frame_model: Some(ctx.frame_model.to_owned()),
            emitter_id: ctx.emitter_id,
            metadata: FrameMetadata {
                frame: Some(index),
                sample_index: Some(info.source_index),
                channel: Some(info.channel),
                channel_hz: Some(
                    ctx.channels_hz
                        .get(usize::from(info.channel))
                        .copied()
                        .unwrap_or(ctx.channel_hz),
                )
                .filter(|f| f.is_finite()),
                bit_len: Some(info.bit_len),
                recipe_version: Some(ctx.recipe_version),
                edit_rev: Some(ctx.edit_rev),
                fec_corrected_bits: Some(info.corrected_bits),
                fit: Some(info.layers.as_ref().map_or(FitStatus::None, |l| l.fit)),
            },
            // The publisher withholds content under a class that forbids it; don't build it then.
            content: class.permits_content().then(|| FrameContent {
                hex: to_hex(&bytes[..bytes.len().min(MAX_FRAME_BYTES)]),
                layers: info.layers.as_deref().cloned(),
            }),
        };
        self.publisher.publish_frame(&rec).map(|_| ())
    }

    fn record(
        &mut self,
        t: Timestamp,
        record_type: InspectorRecordType,
        metadata: Map<String, Value>,
    ) -> Result<(), StreamError> {
        let class = self.publisher.header().content_class;
        self.publisher
            .publish_record(record_type, t, class, &Value::Object(metadata))
            .map(|_| ())
    }

    fn handle(&self) -> PublisherHandle {
        self.handle.clone()
    }

    fn header(&self) -> &StreamHeader {
        self.publisher.header()
    }
}

/// The metadata allowlist of a pipeline's frame streams under a class that forbids content: the
/// §14.2 frame keys the recipe's `output_policy.metadata_keys` names (with fixed, text-free types).
/// `status` and `edit` records are metadata only and go through
/// [`Publisher::publish_record`] as they are.
pub fn inspector_policy(recipe: &Recipe) -> MetadataPolicy {
    let named = |k: &str| {
        recipe
            .output_policy
            .metadata_keys
            .as_ref()
            .is_some_and(|m| m.contains_key(k))
    };
    let mut keys = std::collections::BTreeMap::new();
    let typed = [
        ("frame", MetadataType::Integer),
        ("sample_index", MetadataType::Integer),
        ("channel", MetadataType::Integer),
        ("channel_hz", MetadataType::Number),
        ("bit_len", MetadataType::Integer),
        ("recipe_version", MetadataType::Integer),
        ("edit_rev", MetadataType::Integer),
        ("fec_corrected_bits", MetadataType::Integer),
        (
            "fit",
            MetadataType::Enum(
                ["none", "ok", "partial", "failed"]
                    .map(String::from)
                    .to_vec(),
            ),
        ),
    ];
    for (k, t) in typed {
        if named(k) {
            keys.insert(k.to_owned(), t);
        }
    }
    MetadataPolicy {
        keys,
        frame_models: recipe.output_policy.frame_models.clone(),
        labels: Vec::new(),
        identity: None,
    }
}

/// What the streams of one pipeline share.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamCtx {
    /// Pipeline id.
    pub pipeline_id: String,
    /// Effective class (`clamp(recipe ceiling, source class)`).
    pub class: ContentClass,
    /// Channel centre, Hz.
    pub center_hz: f64,
    /// Channel bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Target emitter.
    pub emitter_id: Option<EmitterId>,
    /// Follow-hops: the channels known at open (empty: the one channel above).
    pub channels: Vec<ChannelInfo>,
    /// What the Listen chooser measured on this channel (T-869, ADR-0015 §12.2), when the
    /// pipeline was started by the `listen` opener rather than by hand. `None` means nothing
    /// measured the channel, and an `audio` header then says so (`mode_rules: recipe-declared`)
    /// instead of inventing a confidence.
    pub measured: Option<std::sync::Arc<crate::recipes::audio::Measured>>,
}

/// Header of a frames stream (inspector output `output_id`, or a frames tap named
/// `<node>.<port>`), with the §14.1 `inspector` profile.
pub fn frames_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    stream_id: String,
    output_id: &str,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        stream_id,
        StreamKind::Messages,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.message_schema = Some(INSPECTOR_MESSAGE_SCHEMA.into());
    h.center_hz = Some(ctx.center_hz);
    h.bandwidth_hz = Some(ctx.bandwidth_hz);
    h.emitter_id = ctx.emitter_id;
    h.inspector = Some(InspectorProfile {
        pipeline_id: ctx.pipeline_id.clone(),
        recipe_id: recipe.id.clone(),
        recipe_version: recipe.version,
        output_id: output_id.to_owned(),
        source: InspectorSource::Live,
        channels: if ctx.channels.is_empty() {
            vec![ChannelInfo {
                index: 0,
                center_hz: ctx.center_hz,
                bandwidth_hz: ctx.bandwidth_hz,
            }]
        } else {
            ctx.channels.clone()
        },
    });
    h
}

/// Header of a stage stream `output_id` on a port of type `ty` at `rate_hz` (§14.4 table).
///
/// `view` selects `raw` (the table's per-port-type `kind`/`datatype`) or `spectrum` (`iq`/`real`
/// ports only; [`tap_spectrum::spectrum_header`]); callers check port-type support before
/// choosing `spectrum` (see `openers::open_stage`).
pub fn stage_header(
    ctx: &StreamCtx,
    recipe: &Recipe,
    stream_id: String,
    output_id: &str,
    ty: PortType,
    rate_hz: f64,
) -> StreamHeader {
    if ty == PortType::Frames {
        return frames_header(ctx, recipe, stream_id, output_id);
    }
    let (kind, datatype) = match ty {
        PortType::Iq => (StreamKind::Iq, "cf32_le"),
        PortType::Real => (StreamKind::Audio, "rf32_le"),
        PortType::Soft => (StreamKind::Symbols, "rf32_le"),
        _ => (StreamKind::Bits, "ru8"),
    };
    let mut h = StreamHeader::new(
        stream_id,
        kind,
        ctx.class,
        format!("hk-pipeline:recipe:{}@{}", recipe.id, recipe.version),
    );
    h.datatype = Some(datatype.into());
    h.sample_rate_hz = Some(rate_hz);
    h.center_hz = Some(ctx.center_hz);
    h.bandwidth_hz = Some(ctx.bandwidth_hz);
    h.emitter_id = ctx.emitter_id;
    h
}

/// Publisher settings of always-on pipeline output streams.
pub fn output_config() -> PublisherConfig {
    PublisherConfig {
        queue_bytes: 4 << 20,
        ..PublisherConfig::default()
    }
}

/// Publisher settings of an on-demand stage tap: one consumer, no drain wait (a closed tap is
/// dropped on the pipeline thread).
pub fn tap_config() -> PublisherConfig {
    PublisherConfig {
        queue_bytes: 8 << 20,
        disconnect_after_drops: u64::MAX,
        disconnect_after: Duration::from_secs(5),
        max_consumers: 1,
        drain_timeout: Duration::ZERO,
    }
}

/// A tap's publisher.
pub enum TapPublisher {
    /// Binary data records (`view=raw`).
    Binary {
        /// The publisher.
        publisher: Publisher,
        /// Encoded payload scratch (pre-sized).
        scratch: Vec<u8>,
    },
    /// Frame records.
    Frames(InspectorSink),
    /// A PSD of an `iq`/`real` port (`view=spectrum`, §14.4, T-160).
    Spectrum {
        /// The publisher.
        publisher: Publisher,
        /// The spectrum engine (buffers samples, averages segments into rows). Boxed: it holds
        /// an FFT plan and several `fft_size`-length scratch buffers, much bigger than the other
        /// variants.
        engine: Box<SpectrumTap>,
        /// Encoded row scratch (pre-sized to `fft_size` `f32`s).
        scratch: Vec<u8>,
    },
    /// A sync-word match-score profile of a `bits` port (`view=sync_search`, §14.4, T-162).
    SyncSearch {
        /// The publisher.
        publisher: Publisher,
        /// The sync-search engine (runs a `SyncCorrelator` over the bits, buffers one row of
        /// scores). Boxed for the same reason as the spectrum engine: its row is bigger than the
        /// other variants' scratch.
        engine: Box<SyncSearchTap>,
        /// Encoded row scratch (pre-sized to the row length in `f32`s).
        scratch: Vec<u8>,
    },
    /// An eye/timing diagram of an `iq`/`real` port (`view=eye`, §14.4, T-161).
    Eye {
        /// The publisher.
        publisher: Publisher,
        /// The eye engine (buffers one row's window, estimates its symbol instants, folds the
        /// waveform around each onto the trace grid). Boxed for the same reason as the others:
        /// its window and row are much bigger than the other variants' state.
        engine: Box<EyeTap>,
        /// Encoded row scratch (pre-sized to the row length in `f32`s).
        scratch: Vec<u8>,
    },
}

impl TapPublisher {
    /// For `header` (frames headers get a frame sink, with the pipeline policy when needed).
    pub fn new(
        header: StreamHeader,
        config: PublisherConfig,
        recipe: &Recipe,
        max_items: usize,
    ) -> Result<Self, StreamError> {
        if header.kind == StreamKind::Messages {
            let policy =
                (!header.content_class.permits_content()).then(|| inspector_policy(recipe));
            return InspectorSink::new(header, config, policy).map(TapPublisher::Frames);
        }
        Ok(TapPublisher::Binary {
            publisher: Publisher::new(header, config)?,
            scratch: Vec::with_capacity(max_items.saturating_mul(8)),
        })
    }

    /// For a `view=spectrum` `header` (built by [`tap_spectrum::spectrum_header`]) at `rate_hz`.
    pub fn new_spectrum(
        header: StreamHeader,
        config: PublisherConfig,
        rate_hz: f64,
    ) -> Result<Self, StreamError> {
        Ok(TapPublisher::Spectrum {
            publisher: Publisher::new(header, config)?,
            engine: Box::new(SpectrumTap::new(rate_hz)),
            scratch: Vec::with_capacity(4 * tap_spectrum::SPECTRUM_FFT_SIZE),
        })
    }

    /// For a `view=sync_search` `header` (built by
    /// [`tap_sync_search::sync_search_header`](crate::recipes::tap_sync_search::sync_search_header))
    /// searching `word`/`bits` over a bits port at `rate_hz`. `None` if `bits`/`word` are invalid
    /// (see [`SyncSearchTap::new`]).
    pub fn new_sync_search(
        header: StreamHeader,
        config: PublisherConfig,
        rate_hz: f64,
        word: u64,
        bits: u32,
    ) -> Result<Option<Self>, StreamError> {
        let Some(engine) = SyncSearchTap::new(rate_hz, word, bits) else {
            return Ok(None);
        };
        let scratch = Vec::with_capacity(4 * engine.row_len());
        Ok(Some(TapPublisher::SyncSearch {
            publisher: Publisher::new(header, config)?,
            engine: Box::new(engine),
            scratch,
        }))
    }

    /// For a `view=eye` `header` (built by
    /// [`tap_eye::eye_header`](crate::recipes::tap_eye::eye_header)) folding an `iq`/`real` port
    /// at `rate_hz` on `symbol_rate_bd` symbols/s. `None` if no eye can be drawn from that
    /// combination (see [`EyeTap::new`]).
    pub fn new_eye(
        header: StreamHeader,
        config: PublisherConfig,
        rate_hz: f64,
        symbol_rate_bd: f64,
    ) -> Result<Option<Self>, StreamError> {
        let Some(engine) = EyeTap::new(rate_hz, symbol_rate_bd) else {
            return Ok(None);
        };
        let scratch = Vec::with_capacity(4 * engine.row_len());
        Ok(Some(TapPublisher::Eye {
            publisher: Publisher::new(header, config)?,
            engine: Box::new(engine),
            scratch,
        }))
    }

    fn handle(&self) -> PublisherHandle {
        match self {
            TapPublisher::Binary { publisher, .. }
            | TapPublisher::Spectrum { publisher, .. }
            | TapPublisher::SyncSearch { publisher, .. }
            | TapPublisher::Eye { publisher, .. } => publisher.handle(),
            TapPublisher::Frames(s) => s.handle(),
        }
    }

    fn header(&self) -> &StreamHeader {
        match self {
            TapPublisher::Binary { publisher, .. }
            | TapPublisher::Spectrum { publisher, .. }
            | TapPublisher::SyncSearch { publisher, .. }
            | TapPublisher::Eye { publisher, .. } => publisher.header(),
            TapPublisher::Frames(s) => s.header(),
        }
    }
}

/// A stage stream on one node output: on demand (with a close flag) or a recipe's declared
/// `stage` output (always on).
pub struct StageTap {
    /// Node id.
    pub node: String,
    /// Output port name.
    pub port: String,
    /// Resolved `(node position, output index)` in the running graph.
    pub(crate) at: Option<(usize, usize)>,
    publisher: TapPublisher,
    handle: PublisherHandle,
    closed: Option<Arc<AtomicBool>>,
    gap: bool,
}

impl StageTap {
    /// A tap on `node.port`; `closed` is set by the session guard of an on-demand tap.
    pub fn new(
        node: String,
        port: String,
        publisher: TapPublisher,
        closed: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            node,
            port,
            at: None,
            handle: publisher.handle(),
            publisher,
            closed,
            gap: true,
        }
    }

    /// The consumer went away.
    pub fn is_closed(&self) -> bool {
        self.closed
            .as_ref()
            .is_some_and(|c| c.load(Ordering::Relaxed))
    }

    /// The stream's handle.
    pub fn handle(&self) -> PublisherHandle {
        self.handle.clone()
    }

    /// The stream's header.
    pub fn header(&self) -> &StreamHeader {
        self.publisher.header()
    }

    /// Publishes the port's current chunk. Returns the frames published.
    pub fn publish(
        &mut self,
        out: &Output,
        ctx: &FrameCtx<'_>,
        t_of: &dyn Fn(f64) -> Timestamp,
    ) -> u64 {
        let restart = out.meta.flags.contains(ChunkFlags::DISCONTINUITY)
            || out.meta.flags.contains(ChunkFlags::RESET);
        match &mut self.publisher {
            TapPublisher::Frames(sink) => publish_frames(sink, out, ctx, t_of),
            TapPublisher::Binary { publisher, scratch } => {
                if self.handle.open_consumers() == 0 {
                    self.gap = true;
                    return 0;
                }
                if out.data.is_empty() {
                    self.gap |= restart;
                    return 0;
                }
                encode(&out.data, scratch);
                let flags = if self.gap || restart {
                    RecordFlags::DISCONTINUITY
                } else {
                    RecordFlags::empty()
                };
                // A gated (content-forbidding class) record is published header-only by the
                // publisher itself; nothing to do here.
                let _ = publisher.publish_binary(BinaryRecord {
                    t: t_of(out.meta.source_index),
                    sample_index: out.meta.index,
                    flags,
                    payload: scratch,
                });
                self.gap = false;
                0
            }
            TapPublisher::Spectrum {
                publisher,
                engine,
                scratch,
            } => {
                if self.handle.open_consumers() == 0 {
                    self.gap = true;
                    // No consumer: don't buffer or FFT samples for a row nobody will read.
                    engine.reset();
                    return 0;
                }
                if out.data.is_empty() {
                    self.gap |= restart;
                    return 0;
                }
                if self.gap || restart {
                    engine.reset();
                }
                self.gap = false;
                let t = t_of(out.meta.source_index);
                let sample_index = out.meta.index;
                let mut emit = |row: &[f32], disc: bool| {
                    tap_spectrum::encode_row(row, scratch);
                    let flags = if disc {
                        RecordFlags::DISCONTINUITY
                    } else {
                        RecordFlags::empty()
                    };
                    // A gated (content-forbidding class) record is published header-only by the
                    // publisher itself; nothing to do here.
                    let _ = publisher.publish_binary(BinaryRecord {
                        t,
                        sample_index,
                        flags,
                        payload: scratch,
                    });
                };
                match &out.data {
                    PortVec::Real(x) => engine.push_real(x, &mut emit),
                    PortVec::Iq(x) => engine.push_iq(x, &mut emit),
                    _ => {}
                }
                0
            }
            TapPublisher::SyncSearch {
                publisher,
                engine,
                scratch,
            } => {
                if self.handle.open_consumers() == 0 {
                    self.gap = true;
                    // No consumer: don't run the correlator or buffer a row nobody will read.
                    engine.reset();
                    return 0;
                }
                if out.data.is_empty() {
                    self.gap |= restart;
                    return 0;
                }
                if self.gap || restart {
                    engine.reset();
                }
                self.gap = false;
                let t = t_of(out.meta.source_index);
                let sample_index = out.meta.index;
                let mut emit = |row: &[f32], disc: bool| {
                    tap_spectrum::encode_row(row, scratch);
                    let flags = if disc {
                        RecordFlags::DISCONTINUITY
                    } else {
                        RecordFlags::empty()
                    };
                    // A gated (content-forbidding class) record is published header-only by the
                    // publisher itself; nothing to do here.
                    let _ = publisher.publish_binary(BinaryRecord {
                        t,
                        sample_index,
                        flags,
                        payload: scratch,
                    });
                };
                if let PortVec::Bits(x) = &out.data {
                    engine.push(x, &mut emit);
                }
                0
            }
            TapPublisher::Eye {
                publisher,
                engine,
                scratch,
            } => {
                if self.handle.open_consumers() == 0 {
                    self.gap = true;
                    // No consumer: don't buffer a window or fold a row nobody will read.
                    engine.reset();
                    return 0;
                }
                if out.data.is_empty() {
                    self.gap |= restart;
                    return 0;
                }
                if self.gap || restart {
                    engine.reset();
                }
                self.gap = false;
                let t = t_of(out.meta.source_index);
                let mut emit = |row: &[f32], disc: bool, first_instant: f64| {
                    tap_spectrum::encode_row(row, scratch);
                    let flags = if disc {
                        RecordFlags::DISCONTINUITY
                    } else {
                        RecordFlags::empty()
                    };
                    // `sample_index` is the port element index of the row's FIRST SYMBOL INSTANT
                    // (§14.4, T-161), not the chunk start: that is how a reader gets the sample
                    // instants the eye was folded on, and lines the eye up against `view=raw`.
                    // The engine estimates it to sub-sample precision and folds the traces on
                    // that; `sample_index` is an integer element index, so the wire carries it
                    // rounded to the nearest port sample (at most half a sample, far below the
                    // trace grid's own step at any sane samples-per-symbol).
                    // A gated (content-forbidding class) record is published header-only by the
                    // publisher itself; nothing to do here.
                    let _ = publisher.publish_binary(BinaryRecord {
                        t,
                        sample_index: first_instant.round().max(0.0) as u64,
                        flags,
                        payload: scratch,
                    });
                };
                match &out.data {
                    PortVec::Real(x) => engine.push_real(x, out.meta.index as f64, &mut emit),
                    PortVec::Iq(x) => engine.push_iq(x, out.meta.index as f64, &mut emit),
                    _ => {}
                }
                0
            }
        }
    }
}

/// Publishes every frame of a `frames` output through `sink`; returns the count.
pub fn publish_frames(
    sink: &mut dyn FrameSink,
    out: &Output,
    ctx: &FrameCtx<'_>,
    t_of: &dyn Fn(f64) -> Timestamp,
) -> u64 {
    let PortVec::Frames(buf) = &out.data else {
        return 0;
    };
    for f in buf.iter() {
        let _ = sink.frame(t_of(f.info.source_index as f64), f.bytes, f.info, ctx);
    }
    buf.len() as u64
}

/// Little-endian payload of a sample-rate port (§14.4 datatypes). Reuses `out`'s capacity.
pub fn encode(data: &PortVec, out: &mut Vec<u8>) {
    out.clear();
    match data {
        PortVec::Iq(x) => {
            for z in x {
                out.extend_from_slice(&z.re.to_le_bytes());
                out.extend_from_slice(&z.im.to_le_bytes());
            }
        }
        PortVec::Real(x) | PortVec::Soft(x) => {
            for v in x {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        PortVec::Bits(x) => out.extend_from_slice(x),
        PortVec::Frames(_) => {}
    }
}

/// One recipe output's stream.
pub enum OutputSink {
    /// An `inspector` output.
    Frames(InspectorSink),
    /// A declared `stage` output (always on).
    Stage(StageTap),
    /// A `messages` output: Decode rows through an off-thread writer (T-111).
    Messages(crate::recipes::messages::MessagesSink),
    /// An `audio` output (T-866): published from the sink node's frames by the runner, not
    /// from a port buffer, so [`OutputSink::publish`] passes it by.
    Audio(crate::recipes::audio::AudioSink),
    /// Nothing served (a placeholder while an edit swaps sinks).
    Idle,
}

impl OutputSink {
    /// Publishes the output's current chunk; returns frames published.
    pub fn publish(
        &mut self,
        out: &Output,
        ctx: &FrameCtx<'_>,
        t_of: &dyn Fn(f64) -> Timestamp,
    ) -> u64 {
        match self {
            OutputSink::Frames(s) => publish_frames(s, out, ctx, t_of),
            OutputSink::Stage(t) => t.publish(out, ctx, t_of),
            OutputSink::Messages(m) => m.publish(out, ctx, t_of),
            OutputSink::Audio(_) | OutputSink::Idle => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex32;

    #[test]
    fn payloads_are_little_endian_and_reuse_capacity() {
        let mut out = Vec::with_capacity(64);
        encode(&PortVec::Iq(vec![Complex32::new(1.0, -2.0)]), &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(&out[..4], &1.0f32.to_le_bytes());
        assert_eq!(&out[4..], &(-2.0f32).to_le_bytes());
        encode(&PortVec::Bits(vec![1, 0, 1]), &mut out);
        assert_eq!(out, [1, 0, 1]);
        assert!(out.capacity() >= 64);
    }
}
