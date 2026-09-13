//! T-041: the STFT was restructured into staging + ordered event replay so batched and
//! asynchronous compute providers can feed it. This test keeps a verbatim copy of the pre-T-041
//! processor loop (over the public `SegmentEngine`) and checks that the new processor — with the
//! CPU reference provider, the multi-threaded CPU provider, and a deliberately delayed provider —
//! emits bit-identical frames (spectrum, SK, holds, time, flags, drops, provenance) and
//! persistence on streams with odd chopping, gaps, drops, retunes, gain and rate changes.

mod common;

use std::collections::VecDeque;

use common::*;
use hk_core::{Discontinuity, ProvenanceHandle};
use hk_dsp::compute::SpectralBackend;
use hk_dsp::synth::{self, Rng};
use hk_dsp::{
    CpuFft, FftBackend, InputInfo, IqSample, Persistence, PersistenceConfig, SegmentEngine,
    SpectrumFrame, StftConfig, StftProcessor, WelchConfig, Window, WindowKind,
};
use hk_model::SampleTime;
use num_complex::Complex32;

/// The pre-T-041 `StftProcessor` (push path only), kept as the reference.
struct OldStft {
    config: StftConfig,
    engine: SegmentEngine,
    seg: Vec<Complex32>,
    fill: usize,
    seg_start: u64,
    next_index: Option<u64>,
    anchor: SampleTime,
    provenance: Option<ProvenanceHandle>,
    frame: Option<SpectrumFrame>,
    frame_start: u64,
    frame_provenance_changed: bool,
    pending_flags: Discontinuity,
    pending_dropped: u64,
    persistence: Option<Persistence>,
}

impl OldStft {
    fn new(config: StftConfig) -> Self {
        let n = config.welch.fft_len;
        Self {
            config,
            engine: SegmentEngine::new(config.welch).unwrap(),
            seg: vec![Complex32::default(); n],
            fill: 0,
            seg_start: 0,
            next_index: None,
            anchor: SampleTime {
                sample_index: 0,
                host_time: hk_model::Timestamp::UNIX_EPOCH,
            },
            provenance: None,
            frame: None,
            frame_start: 0,
            frame_provenance_changed: false,
            pending_flags: Discontinuity::NONE,
            pending_dropped: 0,
            persistence: config.persistence.map(|p| Persistence::new(n, p, 0.0)),
        }
    }

    fn push<T: IqSample>(
        &mut self,
        info: InputInfo<'_>,
        samples: &[T],
        mut emit: impl FnMut(&SpectrumFrame),
    ) {
        self.begin_input(&info);
        let n = self.config.welch.fft_len;
        let overlap = self.config.welch.overlap;
        let hop = n - overlap;
        let first = info.time.sample_index;
        let mut pos = 0;
        while pos < samples.len() {
            if self.fill == 0 {
                self.seg_start = first + pos as u64;
            }
            let take = (n - self.fill).min(samples.len() - pos);
            for (d, &s) in self.seg[self.fill..self.fill + take]
                .iter_mut()
                .zip(&samples[pos..pos + take])
            {
                *d = s.to_complex32();
            }
            self.fill += take;
            pos += take;
            if self.fill == n {
                self.process_segment(&mut emit);
                self.seg.copy_within(hop.., 0);
                self.fill = overlap;
                self.seg_start += hop as u64;
            }
        }
        self.next_index = Some(first + samples.len() as u64);
    }

    fn process_segment(&mut self, emit: &mut impl FnMut(&SpectrumFrame)) {
        if self.engine.count() == 0 {
            self.frame_start = self.seg_start;
            let prov = self.provenance.as_ref().unwrap();
            let frame = self.frame.as_mut().unwrap();
            if frame.provenance != *prov {
                frame.provenance = prov.clone();
            }
        }
        self.engine.process(&self.seg);
        if let Some(p) = &mut self.persistence {
            p.update(self.engine.last_power(), self.engine.per_rbw_offset_db());
        }
        if self.engine.count() as usize == self.config.averages {
            let frame = self.frame.as_mut().unwrap();
            let fs = frame.provenance.tune.sample_rate_hz;
            let fc = frame.provenance.tune.center_hz;
            self.engine.finish_into(fs, fc, &mut frame.spectrum);
            frame.t = SampleTime {
                sample_index: self.frame_start,
                host_time: self.anchor.time_of(self.frame_start, fs),
            };
            frame.sample_count = self.config.frame_samples();
            frame.provenance_changed = self.frame_provenance_changed;
            frame.discontinuity = self.pending_flags;
            frame.dropped_samples = self.pending_dropped;
            emit(frame);
            self.pending_flags = Discontinuity::NONE;
            self.pending_dropped = 0;
            self.frame_provenance_changed = false;
            self.engine.reset();
        }
    }

    fn begin_input(&mut self, info: &InputInfo<'_>) {
        let mut flags = info.discontinuity;
        let mut dropped = 0;
        match self.next_index {
            None => {
                flags |= Discontinuity::STREAM_START;
                dropped = info.dropped_before;
            }
            Some(expected) => {
                let idx = info.time.sample_index;
                if idx != expected {
                    flags |= Discontinuity::GAP;
                    dropped = idx.saturating_sub(expected);
                } else if info.dropped_before > 0 {
                    dropped = info.dropped_before;
                }
            }
        }
        if dropped > 0 {
            flags |= Discontinuity::GAP;
        }
        let prov = info.provenance;
        let rate_changed = match &self.provenance {
            None => true,
            Some(old) if old != prov => {
                let d = Discontinuity::between(old, prov);
                flags |= d;
                let rate = d.contains(Discontinuity::RATE_CHANGE);
                self.provenance = Some(prov.clone());
                if self.engine.count() > 0 {
                    self.frame_provenance_changed = true;
                }
                rate
            }
            Some(_) => false,
        };
        if self.provenance.is_none() {
            self.provenance = Some(prov.clone());
        }
        if self.frame.is_none() {
            self.frame = Some(SpectrumFrame {
                seq: 0,
                t: info.time,
                sample_count: 0,
                provenance: prov.clone(),
                provenance_changed: false,
                discontinuity: Discontinuity::NONE,
                dropped_samples: 0,
                spectrum: self.engine.empty_spectrum(),
            });
        }
        self.anchor = info.time;
        self.pending_flags |= flags;
        self.pending_dropped += dropped;
        if flags.bits() & self.config.reset_on.bits() != 0 {
            self.engine.reset();
            self.fill = 0;
            self.frame_provenance_changed = false;
            let only_gap = flags.bits() & !Discontinuity::GAP.bits() == 0;
            if !only_gap {
                if let Some(p) = &mut self.persistence {
                    p.clear();
                }
            }
        }
        if rate_changed {
            let fs = prov.tune.sample_rate_hz;
            if let Some(p) = &mut self.persistence {
                p.set_frame_period(self.config.welch.hop() as f64 / fs);
            }
        }
    }
}

/// A provider that holds every batch back by `delay` submissions (like a GPU with in-flight
/// work), computing rows with the CPU reference.
struct Delayed {
    fft: CpuFft,
    window: Vec<f32>,
    delay: usize,
    queue: VecDeque<Vec<f32>>,
    max_batch: usize,
}

impl SpectralBackend for Delayed {
    fn name(&self) -> &'static str {
        "test-delayed"
    }
    fn fft_len(&self) -> usize {
        self.window.len()
    }
    fn max_batch(&self) -> usize {
        self.max_batch
    }
    fn submit(
        &mut self,
        span: &[Complex32],
        hop: usize,
        count: usize,
        sink: &mut dyn FnMut(&[f32]),
    ) {
        let n = self.window.len();
        let mut rows = vec![0.0f32; count * n];
        let mut buf = vec![Complex32::default(); n];
        for (s, row) in rows.chunks_exact_mut(n).enumerate() {
            hk_dsp::welch::power_row(
                &mut self.fft,
                &self.window,
                &span[s * hop..s * hop + n],
                &mut buf,
                row,
            );
        }
        self.queue.push_back(rows);
        while self.queue.len() > self.delay {
            let rows = self.queue.pop_front().unwrap();
            sink(&rows);
        }
    }
    fn flush(&mut self, sink: &mut dyn FnMut(&[f32])) {
        while let Some(rows) = self.queue.pop_front() {
            sink(&rows);
        }
    }
    fn in_flight(&self) -> usize {
        self.queue.len()
    }
}

type Frames = Vec<SpectrumFrame>;

fn stream(config: StftConfig, mut new: impl FnMut() -> StftProcessor) -> (Frames, Frames) {
    let fs = 1e6;
    let a = provenance(100e6, fs);
    let retuned = provenance(101e6, fs);
    let gain = provenance_with(101e6, fs, 24.0);
    let rate = provenance(101e6, 2e6);
    let mut rng = Rng::new(17);
    let mut old = OldStft::new(config);
    let mut new_p = new();
    let (mut want, mut got) = (Vec::new(), Vec::new());
    let mut index = 0u64;
    let sizes = [1usize, 7, 333, 4096, 5000, 17, 9000, 1, 2048, 12_345, 600];
    for step in 0..60 {
        let len = sizes[step % sizes.len()];
        let samples = synth::complex_noise(&mut rng, len, 1e-2);
        let prov: &ProvenanceHandle = match step {
            0..=14 => &a,
            15..=29 => &retuned,
            30..=44 => &gain,
            _ => &rate,
        };
        let mut h = header(index, prov, Discontinuity::NONE);
        if step == 9 {
            h.time.sample_index += 777; // gap
            index += 777;
        }
        if step == 20 {
            h.dropped_before = 55;
        }
        let info = InputInfo::from(&h);
        if step % 3 == 0 {
            let (q, _) = synth::quantize_ci8(&samples);
            old.push(info, &q, |f| want.push(f.clone()));
            new_p.push(info, &q, |f| got.push(f.clone()));
        } else {
            old.push(info, &samples, |f| want.push(f.clone()));
            new_p.push(info, &samples, |f| got.push(f.clone()));
        }
        index += len as u64;
    }
    new_p.flush(|f| got.push(f.clone()));
    let snap = |p: Option<&Persistence>| {
        p.map(|p| {
            let mut img = vec![0.0; p.bins() * p.levels()];
            p.write_image(&mut img);
            img
        })
    };
    assert_eq!(
        snap(old.persistence.as_ref()),
        snap(new_p.persistence()),
        "persistence image differs"
    );
    (want, got)
}

fn check(config: StftConfig, new: impl FnMut() -> StftProcessor, label: &str) {
    let (want, got) = stream(config, new);
    assert!(want.len() > 10, "{label}: too few frames ({})", want.len());
    assert_eq!(want.len(), got.len(), "{label}: frame count");
    for (i, (w, g)) in want.iter().zip(&got).enumerate() {
        // seq is new-processor bookkeeping the old copy above does not track.
        let mut w = w.clone();
        w.seq = g.seq;
        assert_eq!(&w, g, "{label}: frame {i} differs");
    }
}

fn configs() -> Vec<StftConfig> {
    let mut with_persistence = StftConfig::new(WelchConfig::new(512), 4);
    with_persistence.persistence = Some(PersistenceConfig::default());
    vec![
        StftConfig::new(WelchConfig::new(1024), 8),
        StftConfig::new(
            WelchConfig {
                overlap: 0,
                window: WindowKind::BlackmanHarris,
                ..WelchConfig::new(256)
            },
            3,
        ),
        StftConfig::new(
            WelchConfig {
                overlap: 700,
                holds: false,
                window: WindowKind::FlatTop,
                ..WelchConfig::new(1000)
            },
            5,
        ),
        with_persistence,
    ]
}

#[test]
fn cpu_reference_matches_pre_t041_processor_bit_for_bit() {
    for config in configs() {
        check(config, || StftProcessor::new(config).unwrap(), "cpu");
    }
}

#[cfg(feature = "cpu-mt")]
#[test]
fn cpu_mt_matches_pre_t041_processor_bit_for_bit() {
    let pool = hk_dsp::compute::pool::shared(None).unwrap();
    for config in configs() {
        check(
            config,
            || {
                let w = Window::new(config.welch.window, config.welch.fft_len);
                StftProcessor::with_spectral(
                    config,
                    Box::new(hk_dsp::compute::CpuMtSpectral::new(&w, pool.clone())),
                )
                .unwrap()
            },
            "cpu-mt",
        );
    }
}

#[test]
fn delayed_provider_matches_pre_t041_processor_bit_for_bit() {
    for config in configs() {
        for (delay, max_batch) in [(1, usize::MAX), (3, 2), (0, 1)] {
            check(
                config,
                || {
                    let n = config.welch.fft_len;
                    let w = Window::new(config.welch.window, n);
                    StftProcessor::with_spectral(
                        config,
                        Box::new(Delayed {
                            fft: CpuFft::new(n),
                            window: w.coefficients().to_vec(),
                            delay,
                            queue: VecDeque::new(),
                            max_batch,
                        }),
                    )
                    .unwrap()
                },
                &format!("delayed {delay}/{max_batch}"),
            );
        }
    }
}

#[test]
fn fft_backend_trait_object_is_used() {
    let config = StftConfig::new(WelchConfig::new(64), 2);
    let p = StftProcessor::with_backend(config, Box::new(CpuFft::new(64))).unwrap();
    assert_eq!(p.backend_name(), "cpu-rustfft");
    assert!(StftProcessor::with_backend(config, Box::new(CpuFft::new(32))).is_err());
    let _: &dyn FftBackend = &CpuFft::new(8);
}
