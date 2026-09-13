//! Plugin chain (ADR-0003, SIGNAL-001): ring → DDC to the plugin's rate → ci8 records →
//! `PluginInstance` (a supervised subprocess; GPL decoders stay behind this boundary) → `Ingest`
//! on its own repository connection (SQLite WAL) → `decodes/<plugin>` messages stream.
//!
//! The DDC is skipped when the channel is centred on the tuned centre at the input rate (the
//! samples are passed through unchanged). The input channel class is the source class, a ceiling
//! on the plugin's output. At detach, `tail_pad_samples` zeros flush block-buffered decoders and
//! the chain waits up to `settle_s` for decodes to settle before stopping the plugin.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use hk_dsp::{Ddc, DdcSpec, InputInfo};
use hk_model::sigmf::Datatype;
use hk_model::{Repository, Timestamp};
use hk_plugins::{
    Ingest, InputStreamDesc, PluginContext, PluginInstance, PluginManifest, PluginState,
};
use hk_stream::{BinaryRecord, Publisher, PublisherConfig, RecordFlags, StreamHeader, StreamKind};
use num_complex::Complex;

use super::{ChainMsg, ChainReader, Next};
use crate::events::Candidate;
use crate::run::Shared;
use crate::stats::{add, inc};

#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    shared: Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    spec_id: &str,
    ddc: Option<(f64, f64)>,
    manifest: &str,
    tail_pad_samples: usize,
    settle_s: f64,
) {
    if let Err(e) = run_inner(&shared, rx, cand, ddc, manifest, tail_pad_samples, settle_s) {
        inc(&shared.counters.chains.attach_errors);
        eprintln!("hk-pipeline: chain {spec_id}: {e:#}");
    }
}

fn to_ci8_bytes(samples: &[Complex<i8>], out: &mut Vec<u8>) {
    out.clear();
    for s in samples {
        out.push(s.re as u8);
        out.push(s.im as u8);
    }
}

fn run_inner(
    shared: &Arc<Shared>,
    rx: Receiver<ChainMsg>,
    cand: Candidate,
    ddc: Option<(f64, f64)>,
    manifest: &str,
    tail_pad_samples: usize,
    settle_s: f64,
) -> anyhow::Result<()> {
    let path = {
        let p = PathBuf::from(manifest);
        if p.is_absolute() {
            p
        } else {
            shared.cfg.manifest_root.join(p)
        }
    };
    let mut m =
        PluginManifest::load(&path).with_context(|| format!("loading {}", path.display()))?;
    if !m.executable.contains('/') {
        if let Some(found) = shared
            .cfg
            .plugin_dirs
            .iter()
            .map(|d| d.join(&m.executable))
            .find(|p| p.is_file())
        {
            m.executable = found.to_string_lossy().into_owned();
        }
    }
    let fs = shared.fs;
    let mut cr = ChainReader::new(Arc::clone(shared), cand.first_sample);
    let first = loop {
        match cr.next() {
            Next::Data(c) => break c,
            Next::Closed => return Ok(()),
            Next::Lost | Next::Idle => {
                if matches!(rx.try_recv(), Ok(ChainMsg::Detach)) {
                    return Ok(());
                }
            }
        }
    };
    let chan_center = 0.5 * (cand.f_lo_hz + cand.f_hi_hz);
    let offset = chan_center - first.provenance.tune.center_hz;
    let (want_rate, bandwidth) = ddc.unwrap_or((fs, fs));
    let identity = (want_rate - fs).abs() < 1.0 && offset.abs() < 1.0;
    let mut ddc_node = if identity {
        None
    } else {
        let mut spec = DdcSpec::new(offset, bandwidth);
        spec.output_rate_hz = Some(want_rate);
        Some(Ddc::new(spec, fs).map_err(|e| anyhow::anyhow!("DDC: {e:?}"))?)
    };
    let out_rate = ddc_node.as_ref().map_or(fs, Ddc::output_rate_hz);

    let class = hk_stream::gate::clamp(shared.cfg.source_class, m.output.content_class);
    let mut header = StreamHeader::new(
        format!("decodes/{}", m.id),
        StreamKind::Messages,
        class,
        format!("hk-plugins:{}@{}", m.id, m.version),
    );
    header.message_schema = Some("hackriff.decode/1".into());
    header.max_frame_len = 64 * 1024;
    let publisher = Publisher::new(
        header.clone(),
        PublisherConfig {
            queue_bytes: 4 << 20,
            ..PublisherConfig::default()
        },
    )?;
    if let Some(sink) = &shared.cfg.stream_sink {
        sink(&header, publisher.handle());
    }
    let repo = Repository::open(&shared.db_path)?;
    let ingest = Arc::new(Mutex::new(Ingest::with_republish(repo, publisher)));
    let input = InputStreamDesc {
        datatype: Datatype::Ci8,
        sample_rate_hz: out_rate,
        center_hz: Some(chan_center),
        bandwidth_hz: Some(bandwidth.min(out_rate)),
        content_class: shared.cfg.source_class,
        anchor: first.time,
        emitter_id: None,
        provenance_ref: None,
    };
    let mut inst = PluginInstance::spawn(m, input, PluginContext::default(), Arc::clone(&ingest))
        .context("spawning the plugin")?;
    // Records offered before the subprocess reads its input are dropped (drop-not-block), so the
    // first push waits for it; meanwhile the chain's gate cursor (lossless replay) or the ring's
    // history (live) holds the samples.
    let started = inst
        .monitor()
        .wait_for(Duration::from_secs(15), |s| s.state == PluginState::Running);
    if !started {
        eprintln!("hk-pipeline: plugin did not reach Running within 15 s; feeding anyway");
    }

    let c = &shared.counters.chains;
    let mut bytes = Vec::new();
    let mut out_index = first.time.sample_index;
    let mut push = |inst: &mut PluginInstance,
                    ddc_node: &mut Option<Ddc>,
                    chunk: &hk_core::ReadChunk,
                    samples: &[Complex<i8>]|
     -> Timestamp {
        match ddc_node {
            None => {
                to_ci8_bytes(samples, &mut bytes);
                out_index = chunk.first_sample();
            }
            Some(d) => match d.process(InputInfo::from(chunk), samples) {
                Ok(block) => {
                    bytes.clear();
                    for z in block.samples {
                        let q = |x: f32| (x * 128.0).round().clamp(-128.0, 127.0) as i8 as u8;
                        bytes.push(q(z.re));
                        bytes.push(q(z.im));
                    }
                }
                Err(_) => bytes.clear(),
            },
        }
        let n = (bytes.len() / 2) as u64;
        if n > 0 {
            let _ = inst.push(BinaryRecord {
                t: chunk.time.host_time,
                sample_index: out_index,
                flags: RecordFlags::empty(),
                payload: &bytes,
            });
            add(&c.plugin_samples, n);
            if ddc_node.is_some() {
                out_index += n;
            }
        }
        chunk.time.host_time
    };
    let first_len = first.len;
    let mut last_t = push(&mut inst, &mut ddc_node, &first, &cr.buf[..first_len]);
    cr.release_to(first.end_sample());
    let mut next_index = first.end_sample();
    loop {
        if matches!(rx.try_recv(), Ok(ChainMsg::Detach)) {
            break;
        }
        match cr.next() {
            Next::Data(ch) => {
                let samples = cr.buf[..ch.len].to_vec();
                last_t = push(&mut inst, &mut ddc_node, &ch, &samples);
                cr.release_to(ch.end_sample());
                next_index = ch.end_sample();
            }
            Next::Lost | Next::Idle => {}
            Next::Closed => break,
        }
    }
    drop(cr);
    if tail_pad_samples > 0 {
        let zeros = vec![0u8; 2 * 4096];
        let mut left = tail_pad_samples;
        let mut idx = if ddc_node.is_some() {
            out_index
        } else {
            next_index
        };
        while left > 0 {
            let n = left.min(4096);
            let _ = inst.push(BinaryRecord {
                t: last_t,
                sample_index: idx,
                flags: RecordFlags::empty(),
                payload: &zeros[..2 * n],
            });
            idx += n as u64;
            left -= n;
        }
    }
    let mon = inst.monitor();
    let deadline = Instant::now() + Duration::from_secs_f64(settle_s.max(0.0));
    let mut last = mon.stats().decodes;
    let mut since = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        let d = mon.stats().decodes;
        if d != last {
            last = d;
            since = Instant::now();
        } else if since.elapsed() >= Duration::from_secs(2) {
            break;
        }
    }
    drop(mon);
    let stats = inst.shutdown();
    let mut ing = ingest.lock().unwrap_or_else(PoisonError::into_inner);
    add(&c.plugin_decodes, ing.stats().decodes_stored);
    add(
        &c.plugin_dropped,
        stats.records_dropped_full + stats.records_dropped_detached,
    );
    add(&c.plugin_restarts, stats.restarts);
    if let Some(p) = ing.take_publisher() {
        p.finish();
    }
    Ok(())
}
