//! `hk-plugin-gnss-sdr`: GNSS-SDR (GPL-3.0-or-later) behind the C22 plugin contract (T-323;
//! `plugins/gnss-sdr/manifest.json`; docs/stream-contract.md §9; ADR-0003, ADR-0010, ADR-0018).
//!
//! **Consumes a dwell, emits evidence.** The wrapper reads the host's `hackriff-v1` framed `ci8`
//! L1 IQ, writes each contiguous run of samples to a scratch file until it holds `--dwell-s`
//! seconds (a gap, a drop marker or end of input closes it early; a run shorter than
//! `--min-dwell-s` is discarded — GPS needs ~30 s of subframes before it has ephemeris), then
//! runs `gnss-sdr --config_file=…` over that file on a worker thread, reads back the RINEX 3
//! observations and NMEA it wrote ([`hk_gnss::receiver`]) and prints:
//!
//! - one `decode` line per observation epoch, `frame_model` `gnss-sdr-epoch`, metadata a
//!   [`ReceiverEpochEvidence`];
//! - one `decode` line per dwell, `frame_model` `gnss-sdr-dwell`, metadata a [`DwellSummary`] —
//!   always, so "ran and saw nothing" differs from "never ran".
//!
//! Never an `identity` (the ingest would upsert an Emitter per PRN) and never an `annotation`
//! (it targets the context's detection): nothing here can create inventory.
//!
//! **Not blocking the host.** Reading input never waits on GNSS-SDR: dwells are handed to the
//! worker over a one-slot channel, and a dwell that completes while the previous one is still
//! being processed is dropped (logged) rather than stalling the reader into the host's hang
//! watchdog. At end of input the last dwell is waited for, then the process exits 0.
//!
//! **Arguments** (all optional): `--gnss-sdr PATH` (default `gnss-sdr`), `--gnss-sdr-arg ARG`
//! (repeatable, passed before `--config_file`), `--dwell-s S` (60), `--min-dwell-s S` (36),
//! `--obs-rate-ms MS` (100), `--channels N` (8), `--timeout-s S` (per run; default
//! `max(120, 4 × dwell)`), `--workdir DIR` (default a per-process directory under the temp dir).
//!
//! GNSS-SDR's own stdout is discarded (it is not NDJSON); its stderr is forwarded, prefixed, up
//! to [`STDERR_LINES_PER_RUN`] lines per run.

use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio, exit};
use std::sync::mpsc::{self, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use hk_gnss::receiver::{
    DwellSummary, FRAME_DWELL, FRAME_EPOCH, GnssSdrRun, MIN_SAMPLE_RATE_HZ, ReceiverEpochEvidence,
    RunOutcome, build_evidence, read_output_dir,
};
use hk_stream::{Record, StreamReader};
use serde_json::json;

const NAME: &str = "hk-plugin-gnss-sdr";
/// Forwarded GNSS-SDR stderr lines per run.
const STDERR_LINES_PER_RUN: usize = 50;
/// `ci8`: one byte I, one byte Q.
const BYTES_PER_SAMPLE: u64 = 2;

struct Args {
    gnss_sdr: String,
    gnss_sdr_args: Vec<String>,
    dwell_s: f64,
    min_dwell_s: f64,
    obs_rate_ms: u32,
    channels: u32,
    timeout_s: Option<f64>,
    workdir: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        gnss_sdr: "gnss-sdr".into(),
        gnss_sdr_args: Vec::new(),
        dwell_s: 60.0,
        min_dwell_s: 36.0,
        obs_rate_ms: 100,
        channels: 8,
        timeout_s: None,
        workdir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        fn num<T: std::str::FromStr>(flag: &str, v: String) -> Result<T, String> {
            v.parse().map_err(|_| format!("{flag}: not a number"))
        }
        match flag.as_str() {
            "--gnss-sdr" => a.gnss_sdr = val()?,
            "--gnss-sdr-arg" => a.gnss_sdr_args.push(val()?),
            "--dwell-s" => a.dwell_s = num(&flag, val()?)?,
            "--min-dwell-s" => a.min_dwell_s = num(&flag, val()?)?,
            "--obs-rate-ms" => a.obs_rate_ms = num(&flag, val()?)?,
            "--channels" => a.channels = num(&flag, val()?)?,
            "--timeout-s" => a.timeout_s = Some(num(&flag, val()?)?),
            "--workdir" => a.workdir = Some(PathBuf::from(val()?)),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if !(a.dwell_s > 0.0 && a.min_dwell_s >= 0.0 && a.min_dwell_s <= a.dwell_s) {
        return Err("need 0 <= --min-dwell-s <= --dwell-s and --dwell-s > 0".into());
    }
    if a.obs_rate_ms == 0 || a.channels == 0 {
        return Err("--obs-rate-ms and --channels must be positive".into());
    }
    Ok(a)
}

/// A completed dwell on disk.
struct Dwell {
    seq: u64,
    path: PathBuf,
    start_sample: u64,
    end_sample: u64,
}

/// The dwell being written.
struct DwellWriter {
    seq: u64,
    path: PathBuf,
    file: io::BufWriter<std::fs::File>,
    start_sample: u64,
    next_sample: u64,
}

impl DwellWriter {
    fn open(dir: &Path, seq: u64, start_sample: u64) -> io::Result<Self> {
        let path = dir.join(format!("dwell-{seq}.ci8"));
        Ok(Self {
            seq,
            file: io::BufWriter::new(std::fs::File::create(&path)?),
            path,
            start_sample,
            next_sample: start_sample,
        })
    }

    fn samples(&self) -> u64 {
        self.next_sample - self.start_sample
    }

    fn close(mut self) -> io::Result<Dwell> {
        self.file.flush()?;
        Ok(Dwell {
            seq: self.seq,
            path: self.path,
            start_sample: self.start_sample,
            end_sample: self.next_sample,
        })
    }
}

struct Worker {
    args_gnss_sdr: String,
    args_extra: Vec<String>,
    workdir: PathBuf,
    rate: f64,
    center: f64,
    obs_rate_ms: u32,
    channels: u32,
    timeout: Duration,
}

impl Worker {
    fn process(&self, d: &Dwell) {
        let out_dir = self.workdir.join(format!("dwell-{}-out", d.seq));
        let (outcome, exit_code) = match std::fs::create_dir_all(&out_dir) {
            Ok(()) => self.run(d, &out_dir),
            Err(e) => {
                eprintln!("{NAME}: creating {}: {e}", out_dir.display());
                (RunOutcome::NotStarted, None)
            }
        };
        let (obs, nmea, rinex_rejected) = read_output_dir(&out_dir);
        let evidence = build_evidence(&obs, &nmea, d.start_sample, d.end_sample);
        let summary = DwellSummary {
            outcome,
            exit_code,
            dwell_start_sample: d.start_sample,
            dwell_end_sample: d.end_sample,
            center_hz: self.center,
            sample_rate_hz: self.rate,
            epochs: evidence.len(),
            max_svs: evidence
                .iter()
                .map(|e| e.epoch.svs.len())
                .max()
                .unwrap_or(0),
            fixes: evidence
                .iter()
                .filter(|e| e.epoch.position.is_some())
                .count(),
            rejected: rinex_rejected + nmea.rejected,
        };
        emit(&evidence, &summary);
        let _ = std::fs::remove_file(&d.path);
        let _ = std::fs::remove_dir_all(&out_dir);
    }

    fn run(&self, d: &Dwell, out_dir: &Path) -> (RunOutcome, Option<i32>) {
        let conf = out_dir.join("gnss-sdr.conf");
        let run = GnssSdrRun {
            input_path: d.path.display().to_string(),
            output_dir: out_dir.display().to_string(),
            sample_rate_hz: self.rate,
            center_hz: self.center,
            channels: self.channels,
            obs_rate_ms: self.obs_rate_ms,
        };
        if let Err(e) = std::fs::write(&conf, run.config_text()) {
            eprintln!("{NAME}: writing config: {e}");
            return (RunOutcome::NotStarted, None);
        }
        let child = Command::new(&self.args_gnss_sdr)
            .args(&self.args_extra)
            .arg(format!("--config_file={}", conf.display()))
            .current_dir(out_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{NAME}: spawning {}: {e}", self.args_gnss_sdr);
                return (RunOutcome::NotStarted, None);
            }
        };
        eprintln!("{NAME}: gnss-sdr pid {} on dwell {}", child.id(), d.seq);
        let stderr = child.stderr.take().expect("piped stderr");
        let pump = thread::spawn(move || {
            for (n, line) in BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
                .enumerate()
            {
                if n < STDERR_LINES_PER_RUN {
                    eprintln!("{NAME}: gnss-sdr: {line}");
                }
            }
        });
        let deadline = Instant::now() + self.timeout;
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    break if status.success() {
                        (RunOutcome::Completed, status.code())
                    } else {
                        (RunOutcome::Failed, status.code())
                    };
                }
                Ok(None) if Instant::now() >= deadline => {
                    eprintln!(
                        "{NAME}: gnss-sdr over its {:?} budget; killed",
                        self.timeout
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    break (RunOutcome::TimedOut, None);
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    eprintln!("{NAME}: waiting for gnss-sdr: {e}");
                    let _ = child.kill();
                    let _ = child.wait();
                    break (RunOutcome::Failed, None);
                }
            }
        };
        let _ = pump.join();
        result
    }
}

fn emit(evidence: &[ReceiverEpochEvidence], summary: &DwellSummary) {
    let mut out = io::stdout().lock();
    for e in evidence {
        let line = json!({
            "type": "decode",
            "sample_index": e.dwell_start_sample,
            "frame_model": FRAME_EPOCH,
            "crc_status": "no-crc",
            "metadata": e,
        });
        let _ = writeln!(out, "{line}");
    }
    let line = json!({
        "type": "decode",
        "sample_index": summary.dwell_start_sample,
        "frame_model": FRAME_DWELL,
        "crc_status": "no-crc",
        "metadata": summary,
    });
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Closes `w` and hands it to the worker, or discards it. `wait` blocks for the worker (end of
/// input); otherwise a busy worker means the dwell is dropped.
fn finish(w: DwellWriter, min_samples: u64, tx: &mpsc::SyncSender<Dwell>, wait: bool, why: &str) {
    let samples = w.samples();
    let d = match w.close() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{NAME}: closing dwell: {e}");
            return;
        }
    };
    if samples < min_samples {
        eprintln!(
            "{NAME}: dwell {} discarded at {why}: {samples} samples, fewer than the {min_samples} minimum",
            d.seq
        );
        let _ = std::fs::remove_file(&d.path);
        return;
    }
    let sent = if wait {
        tx.send(d).map_err(|e| e.0)
    } else {
        match tx.try_send(d) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(d)) => {
                eprintln!("{NAME}: dwell {} dropped: receiver still busy", d.seq);
                Err(d)
            }
            Err(TrySendError::Disconnected(d)) => Err(d),
        }
    };
    if let Err(d) = sent {
        let _ = std::fs::remove_file(&d.path);
    }
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{NAME}: {e}");
            exit(2);
        }
    };
    let mut reader = StreamReader::new(io::stdin().lock());
    let header = match reader.read_header() {
        Ok(h) => h.clone(),
        Err(e) => {
            eprintln!("{NAME}: bad input header: {e}");
            exit(3);
        }
    };
    if header.datatype.as_deref() != Some("ci8") {
        eprintln!(
            "{NAME}: header datatype {:?}, expected ci8",
            header.datatype
        );
        exit(4);
    }
    let (Some(rate), Some(center)) = (header.sample_rate_hz, header.center_hz) else {
        eprintln!("{NAME}: the input header needs sample_rate_hz and center_hz");
        exit(4);
    };
    if rate < MIN_SAMPLE_RATE_HZ {
        eprintln!("{NAME}: {rate} Hz is below the {MIN_SAMPLE_RATE_HZ} Hz L1 C/A needs");
        exit(4);
    }
    let workdir = args.workdir.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("hk-gnss-sdr-{}", std::process::id()))
    });
    if let Err(e) = std::fs::create_dir_all(&workdir) {
        eprintln!("{NAME}: creating {}: {e}", workdir.display());
        exit(5);
    }
    let dwell_samples = (args.dwell_s * rate).round() as u64;
    let min_samples = (args.min_dwell_s * rate).round() as u64;
    let timeout =
        Duration::from_secs_f64(args.timeout_s.unwrap_or((4.0 * args.dwell_s).max(120.0)));

    let worker = Worker {
        args_gnss_sdr: args.gnss_sdr.clone(),
        args_extra: args.gnss_sdr_args.clone(),
        workdir: workdir.clone(),
        rate,
        center,
        obs_rate_ms: args.obs_rate_ms,
        channels: args.channels,
        timeout,
    };
    let (tx, rx) = mpsc::sync_channel::<Dwell>(1);
    let worker_thread = thread::spawn(move || {
        for d in rx {
            worker.process(&d);
        }
    });

    // Buffering to disk needs nothing from GNSS-SDR, so input can be accounted for at once.
    println!("{}", json!({"type": "ready"}));
    let _ = io::stdout().flush();

    let mut seq = 0u64;
    let mut cur: Option<DwellWriter> = None;
    let mut input_error = false;
    loop {
        match reader.next_record() {
            Ok(Some(Record::Binary(b))) => {
                if b.payload.is_empty() {
                    continue;
                }
                let n = b.payload.len() as u64 / BYTES_PER_SAMPLE;
                let idx = b.header.sample_index;
                if cur.as_ref().is_some_and(|w| w.next_sample != idx) {
                    finish(
                        cur.take().expect("checked"),
                        min_samples,
                        &tx,
                        false,
                        "a sample gap",
                    );
                }
                if cur.is_none() {
                    match DwellWriter::open(&workdir, seq, idx) {
                        Ok(w) => cur = Some(w),
                        Err(e) => {
                            eprintln!("{NAME}: opening a dwell file: {e}");
                            input_error = true;
                            break;
                        }
                    }
                    seq += 1;
                }
                let w = cur.as_mut().expect("just opened");
                if let Err(e) = w.file.write_all(&b.payload) {
                    eprintln!("{NAME}: writing the dwell: {e}");
                    input_error = true;
                    break;
                }
                w.next_sample += n;
                if w.samples() >= dwell_samples {
                    finish(
                        cur.take().expect("checked"),
                        min_samples,
                        &tx,
                        false,
                        "full length",
                    );
                }
            }
            Ok(Some(Record::Dropped(_))) => {
                if let Some(w) = cur.take() {
                    finish(w, min_samples, &tx, false, "a drop marker");
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                eprintln!("{NAME}: input error: {e}");
                input_error = true;
                break;
            }
        }
    }
    if let Some(w) = cur.take() {
        finish(w, min_samples, &tx, true, "end of input");
    }
    drop(tx);
    let _ = worker_thread.join();
    if args.workdir.is_none() {
        // Only a directory this process made is removed; a caller's `--workdir` is left alone.
        let _ = std::fs::remove_dir_all(&workdir);
    }
    if input_error {
        exit(3);
    }
}
