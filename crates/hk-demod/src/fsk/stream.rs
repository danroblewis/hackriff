//! Bits stream (C24 `bits`, ADR-0004) for framed bursts: one binary record per burst with a
//! located sync, payload = the burst's bits from the sync start (one byte per bit, `ru8`), in
//! the model polarity. The stream's class is the emitter's effective class (fail closed), and
//! every record goes through the hk-stream egress gate: under a class that forbids content the
//! publisher withholds the payload and sends a header-only `GATED` record.

use hk_estimate::framing::FramingResult;
use hk_model::Bitstream;
use hk_stream::{BinaryRecord, Publisher, RecordFlags, StreamError, StreamHeader, StreamKind};

use super::demod::FSK_DEMOD_VERSION;
use super::receiver::FskBurst;
use super::record::WrittenFraming;

/// Counts of one publish run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BitsPublishStats {
    /// Records published with payload.
    pub published: usize,
    /// Records withheld by the gate (header-only).
    pub gated: usize,
}

/// The stream header for `written`'s Bitstream descriptor.
pub fn bits_stream_header(
    stream_id: &str,
    written: &WrittenFraming,
    bitstream: &Bitstream,
    center_hz: Option<f64>,
) -> StreamHeader {
    let mut h = StreamHeader::new(
        stream_id,
        StreamKind::Bits,
        written.content_class,
        format!("hk-demod:{FSK_DEMOD_VERSION}"),
    );
    h.emitter_id = Some(written.emitter_id);
    h.bitstream_id = Some(bitstream.id);
    h.datatype = Some("ru8".into());
    h.sample_rate_hz = bitstream.framing.symbol_rate_hz;
    h.center_hz = center_hz;
    h.framing = Some(bitstream.framing.clone());
    h
}

/// Publishes one record per framed burst. `ContentGated` outcomes are counted, not errors.
pub fn publish_framed_bits(
    publisher: &mut Publisher,
    bursts: &[FskBurst],
    result: &FramingResult,
) -> Result<BitsPublishStats, StreamError> {
    let mut stats = BitsPublishStats::default();
    for (i, burst) in bursts.iter().enumerate() {
        let (Some(frame), Some(sy)) = (result.frames.get(i), burst.symbols.as_ref()) else {
            continue;
        };
        let Some(at) = frame.sync_bit else {
            continue;
        };
        let payload: Vec<u8> = sy.bits[at..]
            .iter()
            .map(|b| (b & 1) ^ u8::from(frame.inverted))
            .collect();
        let rec = BinaryRecord {
            t: burst.timestamp_of_symbol(at),
            sample_index: sy.source_index[at].max(0.0) as u64,
            flags: RecordFlags::BURST_START.with(RecordFlags::BURST_END),
            payload: &payload,
        };
        match publisher.publish_binary(rec) {
            Ok(_) => stats.published += 1,
            Err(StreamError::ContentGated { .. }) => stats.gated += 1,
            Err(e) => return Err(e),
        }
    }
    Ok(stats)
}
