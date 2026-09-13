//! The capture thread: source → ring, at raised priority (ADR-0001 S1 condition).
//!
//! Reads native ci8 blocks (HackRF, ci8/cu8 recordings; 2 bytes/sample in the ring, and the raw
//! codes the detector's clip counts need). Other datatypes are quantised to ci8 (`round(x·128)`,
//! hk-core's normalisation) and counted. With `--loop` the source is reopened at its end and the
//! stream continues: sample indices and times are shifted past the previous pass and the first
//! block of each pass carries `GAP`, so every reader resets instead of splicing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use hk_core::{Discontinuity, RingWriter, Source, SourceError};
use num_complex::{Complex, Complex32};

use crate::run::{Shared, SourceFactory};
use crate::stats::{add, inc, set};

/// Runs until the source ends (and is not reopened) or `stop` is set; dropping the writer then
/// closes the ring, which ends every reader.
pub(crate) fn run(
    mut source: Box<dyn Source>,
    mut reopen: Option<SourceFactory>,
    mut writer: RingWriter<Complex<i8>>,
    shared: Arc<Shared>,
) -> anyhow::Result<()> {
    let c = &shared.counters.source;
    let mut ci8: Vec<Complex<i8>> = Vec::new();
    let mut f32s: Vec<Complex32> = Vec::new();
    let mut native = true;
    let mut shift: Option<(u64, i64)> = None;
    let mut pass_start = false;
    let (mut end_index, mut end_ns) = (0u64, 0i64);
    let result = loop {
        if shared.stop.load(Ordering::SeqCst) {
            break Ok(());
        }
        let read = if native {
            match source.read_block_ci8(&mut ci8) {
                Err(SourceError::Unsupported { .. }) => {
                    native = false;
                    continue;
                }
                r => r,
            }
        } else {
            match source.read_block(&mut f32s) {
                Ok(Some(h)) => {
                    ci8.clear();
                    ci8.extend(f32s.iter().map(|z| {
                        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8;
                        Complex::new(q(z.re), q(z.im))
                    }));
                    inc(&c.quantised_blocks);
                    Ok(Some(h))
                }
                r => r,
            }
        };
        let mut h = match read {
            Ok(Some(h)) => h,
            Ok(None) => match reopen.as_mut() {
                Some(open) if !shared.stop.load(Ordering::SeqCst) => {
                    source = open()?;
                    native = true;
                    pass_start = true;
                    inc(&c.loops);
                    continue;
                }
                _ => break Ok(()),
            },
            Err(e) => {
                inc(&c.read_errors);
                break Err(anyhow::Error::from(e).context("reading the source"));
            }
        };
        if ci8.is_empty() {
            continue;
        }
        let fs = h.provenance.tune.sample_rate_hz;
        if pass_start {
            pass_start = false;
            let per_sample_ns = (1e9 / fs).round() as i64;
            shift = Some((
                end_index.saturating_sub(h.time.sample_index),
                end_ns + per_sample_ns - h.time.host_time.as_unix_nanos(),
            ));
            h.discontinuity = Discontinuity::from_bits_truncate(
                (h.discontinuity.bits() & !Discontinuity::STREAM_START.bits())
                    | Discontinuity::GAP.bits(),
            );
        }
        if let Some((di, dn)) = shift {
            h.time.sample_index += di;
            h.time.host_time = h.time.host_time.saturating_add_nanos(dn);
        }
        let n = ci8.len() as u64;
        let end = h.time.sample_index + n;
        shared.gate.wait_for_room(end, &shared.stop);
        if writer.push(&h, &ci8).is_err() {
            inc(&c.ring_errors);
            continue;
        }
        add(&c.samples, n);
        inc(&c.blocks);
        add(&c.source_dropped, h.dropped_before);
        set(&c.gate_waits, shared.gate.waits());
        end_index = end;
        end_ns = h.time.host_time.as_unix_nanos() + (n as f64 * 1e9 / fs).round() as i64;
        let counters = &shared.counters;
        counters.stream_time_ns.store(end_ns, Ordering::Relaxed);
        counters
            .tune_center_bits
            .store(h.provenance.tune.center_hz.to_bits(), Ordering::Relaxed);
        counters
            .tune_rate_bits
            .store(fs.to_bits(), Ordering::Relaxed);
    };
    drop(writer);
    result
}
