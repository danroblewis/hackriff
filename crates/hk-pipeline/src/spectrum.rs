//! Always-on reader 3: the live spectrum stream (C24, ADR-0004).
//!
//! Its own STFT at `spectrum_fft_len` bins and at most `spectrum_rows_per_s` rows ([`row_plan`]),
//! PSD rows in dBFS/Hz as little-endian f32 (`rf32_le`), published drop-not-block under the
//! source class ([`crate::class`]): a class that forbids content gates spectrum at ≤ 50 rows/s
//! (the publisher enforces it; withheld rows are counted). The publisher is offered to
//! [`crate::config::StreamSink`] (the hk-api bridge registry).

use std::sync::Arc;
use std::time::Duration;

use hk_core::{Discontinuity, ReadOutcome};
use hk_dsp::{InputInfo, PowerUnit, StftProcessor};
use hk_stream::{BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamError};
use num_complex::Complex;

use crate::class::{row_plan, spectrum_header};
use crate::run::Shared;
use crate::stats::{add, inc, set};

/// Runs reader 3 until the ring closes.
pub(crate) fn run(shared: Arc<Shared>) -> anyhow::Result<()> {
    let s = &shared.cfg.settings;
    let class = shared.cfg.source_class;
    let plan = row_plan(shared.fs, s.spectrum_fft_len, s.spectrum_rows_per_s, class);
    let bins = plan.stft.welch.fft_len;
    let mut stft =
        StftProcessor::new(plan.stft).map_err(|e| anyhow::anyhow!("spectrum STFT: {e:?}"))?;
    let mut reader = shared.ring.reader_at(0);
    let cursor = shared.gate.register(0);
    let mut buf = vec![Complex::<i8>::default(); 1 << 16];
    let rc = &shared.counters.spectrum_reader;
    let sc = &shared.counters.spectrum;
    let mut publisher: Option<Publisher> = None;
    let mut db = vec![0f32; bins];
    let mut bytes = vec![0u8; 4 * bins];
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(chunk) => {
                if publisher.is_none() {
                    let header = spectrum_header(
                        &shared.cfg.spectrum_stream_id,
                        "hk-pipeline:spectrum",
                        class,
                        &plan,
                        chunk.provenance.tune.center_hz,
                        shared.fs,
                    );
                    let p = Publisher::new(
                        header.clone(),
                        PublisherConfig {
                            queue_bytes: (1 << 20).max(64 * (32 + 4 * bins)),
                            ..PublisherConfig::default()
                        },
                    )?;
                    if let Some(sink) = &shared.cfg.stream_sink {
                        sink(&header, p.handle());
                    }
                    publisher = Some(p);
                }
                let p = publisher.as_mut().expect("publisher");
                stft.push(InputInfo::from(&chunk), &buf[..chunk.len], |frame| {
                    let spec = &frame.spectrum;
                    if spec.bins() != bins {
                        return;
                    }
                    spec.write_db(&spec.psd, PowerUnit::DbfsPerHz, &mut db);
                    for (c, v) in bytes.chunks_exact_mut(4).zip(&db) {
                        c.copy_from_slice(&v.to_le_bytes());
                    }
                    let mut flags = RecordFlags::empty();
                    if frame.provenance.get().overload {
                        flags = flags.with(RecordFlags::OVERLOAD);
                    }
                    if frame.discontinuity.bits() & !Discontinuity::STREAM_START.bits() != 0 {
                        flags = flags.with(RecordFlags::DISCONTINUITY);
                    }
                    inc(&sc.rows);
                    match p.publish_binary(BinaryRecord {
                        t: frame.t.host_time,
                        sample_index: frame.t.sample_index,
                        flags,
                        payload: &bytes,
                    }) {
                        Ok(_) => {}
                        Err(StreamError::SpectrumGated { .. }) => inc(&sc.rows_gated),
                        Err(_) => inc(&sc.errors),
                    }
                });
                cursor.set(chunk.end_sample());
                add(&rc.samples, chunk.len as u64);
            }
            ReadOutcome::Overrun { .. } | ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
        set(&rc.lost_samples, reader.lost_samples());
        set(&rc.overruns, reader.overruns());
        set(&rc.gap_samples, reader.gap_samples());
        let st = stft.stats();
        set(&rc.frames, st.frames);
        set(&rc.stft_resets, st.resets);
    }
    drop(cursor);
    if let Some(p) = publisher {
        p.finish();
    }
    Ok(())
}
