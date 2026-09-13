//! `hk-dummy-plugin`: the T-014 test decoder plugin (`plugins/dummy/manifest.json`).
//!
//! Reads the input stream on stdin (hackriff-v1 framing, or raw fixed-size records with
//! `--raw-record-bytes`), and writes one NDJSON `decode` line to stdout per `--every` records.
//!
//! Test switches:
//! - `--profile adsb-like`: decodes name ICAO addresses `a1b2c0`..`a1b2c3` (ADS-B-like, SIGNAL-001).
//! - `--crash-after K`: exit with code 101 after K records (crash isolation).
//! - `--stall`: never read stdin (hang watchdog, backpressure).
//! - `--claim-class C` / `--content TEXT`: claim a class and attach content (clamping, gating).
//! - `--annotate`: also emit an `annotation` line per message.
//! - `--datatype D`: exit with code 4 unless the header's datatype is D.

use std::io::{self, BufWriter, Read, Write};
use std::process::exit;
use std::time::Duration;

use hk_api::stream::{Record, StreamReader};
use serde_json::json;

#[derive(PartialEq)]
enum Profile {
    Generic,
    AdsbLike,
}

struct Args {
    every: u64,
    crash_after: Option<u64>,
    profile: Profile,
    claim_class: Option<String>,
    content: Option<String>,
    stall: bool,
    annotate: bool,
    datatype: Option<String>,
    raw_record_bytes: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        every: 10,
        crash_after: None,
        profile: Profile::Generic,
        claim_class: None,
        content: None,
        stall: false,
        annotate: false,
        datatype: None,
        raw_record_bytes: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--every" => {
                args.every = value()?.parse().map_err(|e| format!("--every: {e}"))?;
                if args.every == 0 {
                    return Err("--every must be > 0".into());
                }
            }
            "--crash-after" => {
                args.crash_after = Some(
                    value()?
                        .parse()
                        .map_err(|e| format!("--crash-after: {e}"))?,
                )
            }
            "--profile" => {
                args.profile = match value()?.as_str() {
                    "generic" => Profile::Generic,
                    "adsb-like" => Profile::AdsbLike,
                    other => return Err(format!("unknown profile {other:?}")),
                }
            }
            "--claim-class" => args.claim_class = Some(value()?),
            "--content" => args.content = Some(value()?),
            "--datatype" => args.datatype = Some(value()?),
            "--raw-record-bytes" => {
                args.raw_record_bytes = Some(
                    value()?
                        .parse()
                        .map_err(|e| format!("--raw-record-bytes: {e}"))?,
                )
            }
            "--stall" => args.stall = true,
            "--annotate" => args.annotate = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(args)
}

#[derive(Default)]
struct State {
    records: u64,
    payload_bytes: u64,
    dropped_seen: u64,
}

fn emit(args: &Args, state: &State, out: &mut impl Write, sample_index: u64) -> io::Result<()> {
    let n = state.records / args.every;
    let (mut decode, label) = match args.profile {
        Profile::Generic => (
            json!({
                "type": "decode",
                "sample_index": sample_index,
                "frame_model": "dummy-summary",
                "crc_status": "no-crc",
                "metadata": {
                    "records": state.records,
                    "payload_bytes": state.payload_bytes,
                    "dropped_seen": state.dropped_seen
                }
            }),
            "dummy",
        ),
        Profile::AdsbLike => {
            let icao = format!("{:06x}", 0xa1b2c0 + (n - 1) % 4);
            (
                json!({
                    "type": "decode",
                    "sample_index": sample_index,
                    "frame_model": "adsb-df17",
                    "crc_status": "valid",
                    "identity": {"scheme": "adsb-icao", "value": icao},
                    "metadata": {"icao": icao, "df": 17, "crc": "ok", "records": state.records}
                }),
                "adsb",
            )
        }
    };
    let mut annotation = json!({
        "type": "annotation",
        "value": label,
        "kind": "ground-truth",
        "confidence": 1.0,
        "sample_index": sample_index,
        "metadata": {"records": state.records}
    });
    for line in [&mut decode, &mut annotation] {
        if let Some(class) = &args.claim_class {
            line["content_class"] = json!(class);
        }
        if let Some(text) = &args.content {
            line["content"] = json!({"text": text, "n": n});
        }
    }
    writeln!(out, "{decode}")?;
    if args.annotate {
        writeln!(out, "{annotation}")?;
    }
    out.flush()
}

fn on_record(
    args: &Args,
    state: &mut State,
    out: &mut impl Write,
    sample_index: u64,
    len: usize,
) -> io::Result<()> {
    state.records += 1;
    state.payload_bytes += len as u64;
    if state.records % args.every == 0 {
        emit(args, state, out, sample_index)?;
    }
    if args.crash_after == Some(state.records) {
        out.flush()?;
        eprintln!("hk-dummy-plugin: crashing after {} records", state.records);
        exit(101);
    }
    Ok(())
}

fn run(args: &Args) -> io::Result<u64> {
    let mut out = BufWriter::new(io::stdout().lock());
    let mut state = State::default();
    let stdin = io::stdin().lock();
    if let Some(size) = args.raw_record_bytes {
        let mut stdin = stdin;
        let mut buf = vec![0u8; size.max(1)];
        let mut index = 0u64;
        while stdin.read_exact(&mut buf).is_ok() {
            on_record(args, &mut state, &mut out, index, size)?;
            index += size as u64;
        }
        return Ok(state.records);
    }
    let mut reader = StreamReader::new(stdin);
    let header = match reader.read_header() {
        Ok(h) => h.clone(),
        Err(e) => {
            eprintln!("hk-dummy-plugin: bad input header: {e}");
            exit(3);
        }
    };
    if let Some(expected) = &args.datatype
        && header.datatype.as_deref() != Some(expected)
    {
        eprintln!(
            "hk-dummy-plugin: header datatype {:?}, expected {expected}",
            header.datatype
        );
        exit(4);
    }
    loop {
        match reader.next_record() {
            Ok(Some(Record::Binary(b))) => on_record(
                args,
                &mut state,
                &mut out,
                b.header.sample_index,
                b.payload.len(),
            )?,
            Ok(Some(Record::Dropped(d))) => state.dropped_seen += d.count,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                eprintln!("hk-dummy-plugin: input error: {e}");
                exit(5);
            }
        }
    }
    Ok(state.records)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hk-dummy-plugin: {e}");
            exit(2);
        }
    };
    eprintln!("hk-dummy-plugin: started (every {})", args.every);
    if args.stall {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    match run(&args) {
        Ok(records) => eprintln!("hk-dummy-plugin: stdin closed after {records} records"),
        Err(e) => {
            eprintln!("hk-dummy-plugin: output error: {e}");
            exit(6);
        }
    }
}
