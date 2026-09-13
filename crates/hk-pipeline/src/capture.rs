//! The capture thread: source → ring, at raised priority (ADR-0001 S1 condition).
//!
//! Reads native ci8 blocks (HackRF, ci8/cu8 recordings; 2 bytes/sample in the ring, and the raw
//! codes the detector's clip counts need). Other datatypes are quantised to ci8 (`round(x·128)`,
//! hk-core's normalisation) and counted. With `--loop` the source is reopened at its end and the
//! stream continues: sample indices and times are shifted past the previous pass and the first
//! block of each pass carries `GAP`, so every reader resets instead of splicing.
//!
//! - **Lossless gate:** each block waits in [`crate::gate::FlowGate::wait_for_block`] with the
//!   ring's oldest and newest positions, so claims below a recording's `core:global_index` never
//!   hold it. A reopened source must still be pausable.
//! - **Virtual tuning is marked:** when the scheduler drives a replay, gain and filter changes
//!   exist only in the provenance (the samples never had them). Every block whose gains or filter
//!   differ from the first block's gets its provenance's `device_id` suffixed with
//!   [`crate::control::VIRTUAL_TUNING_DEVICE_SUFFIX`], so detections and recordings never claim a
//!   real gain state the recording did not have. (A multi-capture recording whose real gains
//!   change is over-marked, which is the safe direction.)
//! - **Coverage hold (lossless, T-037b):** after a block changes the tuned window, the capture
//!   thread publishes the tune (`tune_seq`) and, when coverage chains exist, waits until the
//!   chain manager has polled coverage for it (`coverage_seq`). A coverage chain therefore
//!   attaches and claims its samples in the flow gate before a replay shorter than the ring can
//!   be written through and closed (a 0.1 s ADS-B scene used to end with no chain). The hold is
//!   bounded by [`COVERAGE_WAIT`] and by `stop`.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use hk_core::source::SourceKind;
use hk_core::{Discontinuity, ProvenanceHandle, RingWriter, Source, SourceError};
use hk_model::ProvenanceId;
use num_complex::{Complex, Complex32};

use crate::chains::spec::Trigger;
use crate::control::VIRTUAL_TUNING_DEVICE_SUFFIX;
use crate::run::{Shared, SourceFactory};
use crate::stats::{Counters, add, inc, set};

/// Longest coverage hold per tune change.
pub(crate) const COVERAGE_WAIT: Duration = Duration::from_secs(10);

/// Waits until the chain manager has evaluated coverage for tune `seq` (see the module docs).
fn wait_for_coverage(shared: &Shared, seq: u64) {
    let c = &shared.counters;
    if c.coverage_seq.load(Ordering::SeqCst) >= seq {
        return;
    }
    inc(&c.source.coverage_waits);
    let deadline = Instant::now() + COVERAGE_WAIT;
    while c.coverage_seq.load(Ordering::SeqCst) < seq {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        if Instant::now() >= deadline {
            inc(&c.source.coverage_wait_timeouts);
            return;
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// Provenance marks for a scheduler-driven replay (see the module docs).
#[derive(Default)]
struct VirtualMarks {
    base: Option<[u64; 4]>,
    marked: Vec<(ProvenanceId, ProvenanceHandle)>,
}

impl VirtualMarks {
    fn apply(&mut self, prov: &mut ProvenanceHandle, counters: &Counters) {
        let t = &prov.get().tune;
        let key = [
            t.lna_db.to_bits(),
            t.vga_db.to_bits(),
            u64::from(t.amp_on),
            t.bandwidth_hz.to_bits(),
        ];
        if *self.base.get_or_insert(key) == key {
            return;
        }
        let id = prov.id();
        if let Some((_, m)) = self.marked.iter().find(|(i, _)| *i == id) {
            *prov = m.clone();
            return;
        }
        let mut record = prov.get().clone();
        record.device_id = format!("{}{VIRTUAL_TUNING_DEVICE_SUFFIX}", record.device_id);
        let m = ProvenanceHandle::new(record);
        if self.marked.len() >= 256 {
            self.marked.clear();
        }
        self.marked.push((id, m.clone()));
        inc(&counters.scheduler.virtual_provenances);
        *prov = m;
    }
}

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
    let mut marks = (shared.cfg.drive_scheduler
        && source.capabilities().kind == SourceKind::Replay)
        .then(VirtualMarks::default);
    let hold_for_coverage =
        shared.gate.enabled() && shared.specs.iter().any(|s| s.trigger == Trigger::Coverage);
    let mut last_tune: Option<(u64, u64)> = None;
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
                    if shared.gate.enabled() && !source.pausable() {
                        break Err(anyhow::anyhow!(
                            "the reopened source ({}) cannot pause, so it cannot run lossless",
                            source.capabilities().driver
                        ));
                    }
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
        if let Some(m) = marks.as_mut() {
            m.apply(&mut h.provenance, &shared.counters);
        }
        let n = ci8.len() as u64;
        let end = h.time.sample_index + n;
        if shared.gate.enabled() {
            shared.gate.wait_for_block(
                h.time.sample_index,
                end,
                shared.ring.oldest_sample(),
                shared.ring.next_sample(),
                &shared.stop,
            );
        }
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
        // Published after the tune itself, so a poll never acknowledges a tune it did not see.
        let tune = (h.provenance.tune.center_hz.to_bits(), fs.to_bits());
        if last_tune != Some(tune) {
            last_tune = Some(tune);
            let seq = counters.tune_seq.fetch_add(1, Ordering::SeqCst) + 1;
            if hold_for_coverage {
                wait_for_coverage(&shared, seq);
            }
        }
    };
    drop(writer);
    result
}

#[cfg(test)]
mod tests {
    use hk_model::{ClockSource, Provenance, TimestampMethod, Tune};

    use super::*;

    fn provenance(lna_db: f64) -> ProvenanceHandle {
        ProvenanceHandle::new(Provenance {
            device_id: "sigmf:hackrf".into(),
            tune: Tune {
                center_hz: 433.92e6,
                sample_rate_hz: 2e6,
                lna_db,
                vga_db: 20.0,
                amp_on: false,
                bandwidth_hz: 1.75e6,
            },
            overload: false,
            quantisation_limited: false,
            temperature_c: None,
            antenna_port: None,
            clock_source: ClockSource::Internal,
            clock_locked: true,
            calibration_state_ref: None,
            spur_mask_ref: None,
            timestamp_method: TimestampMethod::Unknown,
            timestamp_error_budget_ns: None,
        })
    }

    #[test]
    fn virtual_gain_changes_are_marked_and_the_recorded_state_is_not() {
        let counters = Counters::default();
        let mut marks = VirtualMarks::default();
        let real = provenance(24.0);
        let mut p = real.clone();
        marks.apply(&mut p, &counters);
        assert_eq!(
            p.id(),
            real.id(),
            "the recording's own gain state is unchanged"
        );
        let stepped = provenance(32.0);
        let mut a = stepped.clone();
        marks.apply(&mut a, &counters);
        assert_eq!(a.device_id, "sigmf:hackrf+virtual-tuning");
        assert_eq!(a.tune.lna_db, 32.0);
        let mut b = stepped.clone();
        marks.apply(&mut b, &counters);
        assert_eq!(a.id(), b.id(), "one marked handle per source handle");
        let mut back = real.clone();
        marks.apply(&mut back, &counters);
        assert_eq!(back.device_id, "sigmf:hackrf");
        assert_eq!(
            counters
                .scheduler
                .virtual_provenances
                .load(Ordering::Relaxed),
            1
        );
    }
}
