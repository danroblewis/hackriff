//! T-057 (spectrum part) and T-050: the spectrum stream header follows a mid-stream tune applied
//! through the control plane (`PipelineController::retune`, in place: same class and rate). A
//! scripted radio plays a tone at +50 kHz while tuned to `A` and at -50 kHz while tuned to `B`;
//! a consumer of every offered spectrum publisher checks that each row's peak sits where its own
//! header's centre says, and that no row of the new window was sent under the old header.

mod common;
#[path = "support/radio.rs"]
mod radio;

use std::io::{Cursor, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use hk_model::{ContentClass, Timestamp};
use hk_pipeline::class::window_class;
use hk_pipeline::{Pipeline, PipelineConfig, SourceInfo, TrackInventory, replay_plan};
use hk_stream::{Declared, Record, StreamHeader, StreamReader};

const FS: f64 = 250e3;
const A: f64 = 100.5e6;
const B: f64 = 101.5e6;

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn wait(what: &str, limit: Duration, f: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_mid_stream_tune_re_offers_the_header_before_any_row_of_the_new_window() {
    assert_eq!(window_class(A, FS), ContentClass::Unrestricted);
    assert_eq!(window_class(B, FS), ContentClass::Unrestricted);
    let dir = TempDir::new("t057-header");
    let (radio, ctl) = radio::Radio::new(
        A,
        FS,
        4096,
        radio::tone(|center| if center == A { 50e3 } else { -50e3 }),
    );
    let t0 = Timestamp::from_unix_nanos(radio::T0_NS);
    let mut cfg = PipelineConfig::new(&dir.0, replay_plan(A, FS, t0)).unwrap();
    cfg.source_class = window_class(A, FS);
    cfg.live_window_class = true;
    cfg.lossless = true;
    cfg.settings.chains = Some(Vec::new());
    cfg.settings.spectrum_fft_len = 256;
    let streams: Arc<Mutex<Vec<(StreamHeader, Buf)>>> = Arc::default();
    let seen = Arc::clone(&streams);
    let id = cfg.spectrum_stream_id.clone();
    cfg.stream_sink = Some(Arc::new(move |h, handle| {
        if h.stream_id != id {
            return;
        }
        let buf = Buf::default();
        handle
            .subscribe("t057", Declared::local(buf.clone()), Box::new(|_| {}))
            .expect("subscribe");
        seen.lock().unwrap().push((h.clone(), buf));
    }));
    let half = (FS / 2.0) as u64;
    ctl.hold_at(half);
    let handle = Pipeline::start(
        cfg,
        Box::new(radio),
        SourceInfo {
            sample_rate_hz: FS,
            center_hz: A,
            start_time: t0,
        },
        None,
        Box::new(TrackInventory::default()),
    )
    .unwrap();
    let counters = handle.counters();
    let spectrum_read = || counters.spectrum_reader.samples.load(Ordering::Relaxed);
    assert!(ctl.wait_emitted(half, Duration::from_secs(60)));
    wait("the first window's rows", Duration::from_secs(60), || {
        spectrum_read() >= half
    });
    let out = handle.controller().retune(B, FS).expect("retune");
    assert!(!out.replumbed, "same class and rate: tuned in place");
    ctl.hold_at(2 * half);
    assert!(ctl.wait_emitted(2 * half, Duration::from_secs(60)));
    wait("the second window's rows", Duration::from_secs(60), || {
        spectrum_read() >= 2 * half
    });
    ctl.finish();
    let (summary, fired) = wait_guarded(handle, Duration::from_secs(120));
    eprintln!("{}", summary.to_text());
    assert!(!fired, "the run finished on its own");
    assert!(summary.errors.is_empty(), "{:?}", summary.errors);

    let windows = ctl.windows.lock().unwrap().clone();
    assert_eq!(windows.len(), 2, "{windows:?}");
    let (tune_index, center, _) = windows[1];
    assert_eq!(center, B);
    let streams = streams.lock().unwrap().clone();
    let centres: Vec<Option<f64>> = streams.iter().map(|(h, _)| h.center_hz).collect();
    assert_eq!(
        centres,
        vec![Some(A), Some(B)],
        "one header per window, in order"
    );
    for (header, buf) in &streams {
        let bins = header.fft_size.unwrap() as usize;
        let span = header.bandwidth_hz.unwrap();
        assert_eq!((bins, span), (256, FS));
        let bytes = buf.0.lock().unwrap().clone();
        let mut reader = StreamReader::new(Cursor::new(bytes));
        assert_eq!(reader.read_header().unwrap().center_hz, header.center_hz);
        let (mut rows, mut discontinuities) = (0, 0);
        while let Some(rec) = reader.next_record().unwrap() {
            let Record::Binary(b) = rec else { continue };
            if b.payload.len() != 4 * bins {
                continue;
            }
            rows += 1;
            let psd: Vec<f32> = b
                .payload
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let peak = psd
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0;
            let offset = (peak as f64 - bins as f64 / 2.0) * FS / bins as f64;
            let (want, starts_ok) = if header.center_hz == Some(A) {
                (50e3, b.header.sample_index < tune_index)
            } else {
                (-50e3, b.header.sample_index >= tune_index)
            };
            assert!(
                (offset - want).abs() <= 2.0 * FS / bins as f64,
                "a row under the header centred at {:?} peaks at {offset} Hz, not {want} Hz \
                 (sample {}, tune at {tune_index})",
                header.center_hz,
                b.header.sample_index
            );
            assert!(
                starts_ok,
                "row at sample {} is under the wrong header (tune at {tune_index})",
                b.header.sample_index
            );
            if b.header.flags.0 & hk_stream::RecordFlags::DISCONTINUITY.0 != 0 {
                discontinuities += 1;
            }
        }
        assert!(rows >= 5, "{:?}: {rows} rows", header.center_hz);
        eprintln!(
            "header {:?}: {rows} rows ({discontinuities} flagged discontinuity)",
            header.center_hz
        );
    }
}
