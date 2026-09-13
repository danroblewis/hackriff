//! `hk-plugin-readsb`: the T-015 ADS-B / Mode S decoder plugin wrapper
//! (`plugins/readsb/manifest.json`), fitting readsb (GPL-3.0-or-later) behind the T-014 plugin
//! contract (docs/stream-contract.md §9, §9.6; ADR-0003).
//!
//! readsb is exec'd as a child, never linked: the process boundary is the licence boundary
//! (ADR-0010). This binary is the "thin wrapper" docs/stream-contract.md §9.6 anticipated:
//!
//! - **Framing.** The manifest declares `hackriff-v1` framing (the default), not `raw`, so this
//!   wrapper — not readsb — sees the plugin host's binary record headers. That is what lets it
//!   recover a sample-time anchor for each readsb message (see "Timestamps" below); a `raw`-framed
//!   byte stream would hide record boundaries entirely.
//! - **Format conversion.** The channel carries `ci8` (signed, HackRF-native); readsb's `ifile`
//!   reader wants `UC8` (offset-binary unsigned). Each byte is XORed with `0x80`
//!   (`ci8 -> uc8` is exactly a 128-count offset, i.e. sign-bit flip) and piped to readsb's stdin
//!   as it arrives — no full-buffer copy, so capture is never blocked on readsb's pace.
//! - **readsb invocation.** `--device-type ifile --ifile - --iformat UC8 --no-interactive --raw
//!   --no-fix`. `--no-fix` disables the single-bit CRC correction: every line readsb prints on a
//!   `--raw` stream is then genuinely CRC-valid (docs/stream-contract.md §9.6's "readsb only
//!   prints CRC-valid or repaired frames — record which" is resolved by not letting it repair
//!   anything). This wrapper still independently recomputes the CRC-24 remainder before emitting
//!   a decode, as a defence against a corrupted line.
//! - **Message parsing.** DF17 extended squitters (identification, TC 1-4; airborne position,
//!   TC 9-18, paired even/odd CPR frames per ICAO; velocity, TC 19, ground-speed subtype) and
//!   DF11 all-call replies (ICAO only — no ME field). Other downlink formats carry the ICAO
//!   folded into the parity field (address/parity XOR) and are out of scope for this wrapper.
//! - **Timestamps.** `sample_index` on every emitted decode line is the sample index *most
//!   recently written to readsb's stdin* at the moment its message line is read back — the
//!   fallback docs/stream-contract.md §9.6 anticipated, since raw AVR output carries no
//!   correlatable position in the input stream. This is accurate to within one input record's
//!   worth of samples (± the block latency between writing to readsb and it echoing a message for
//!   those bytes); the host turns it into a timestamp via the input stream's anchor and rate
//!   ([`crate::output::parse_line`] — but see `crates/hk-plugins/src/output.rs` in the sibling
//!   library crate; this binary only emits the NDJSON, it does not link that module).
//! - **Crash isolation.** readsb runs as an ordinary child (not detached), inside this process's
//!   own process group, which the plugin host already owns and can SIGKILL as a whole (`kill_group`
//!   in `host.rs`). If readsb exits on its own — killed out from under this wrapper, or a crash —
//!   a waiter thread notices and this process exits non-zero, so the host's supervisor treats the
//!   *pair* as one crashed plugin instance and restarts both together. A clean shutdown (this
//!   wrapper's own stdin reaching EOF) closes readsb's stdin first and waits for its exit instead.
//!   readsb's own quirk: reading an `ifile` to EOF always logs "Abnormal exit" and returns a
//!   non-zero, non-signalled status — that is normal completion, not a crash; only a signalled
//!   exit (SIGKILL/SIGTERM) counts as one.
//! - **Own pid, for tests.** `hk-plugin-readsb: readsb pid <pid>` is logged to stderr (captured in
//!   the host's log ring under a content-permitting ceiling), the same convention
//!   `hk-dummy-plugin` uses for its `--orphan`/`--stall-child` grandchildren, so a test can find
//!   and signal the real readsb process.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio, exit};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use hk_stream::{Record, StreamReader};
use serde_json::{Map, Value, json};

/// Mode S CRC-24 generator polynomial (docs/04; matches `py/hkpy/synth/adsb.py`).
const CRC24_GENERATOR: u32 = 0x01FF_F409;

/// Reverse-lookup charset for the 6-bit identification characters (ICAO Annex 10).
const CHARSET: &[u8; 64] = b"#ABCDEFGHIJKLMNOPQRSTUVWXYZ##### ###############0123456789######";

/// `2^17`, the CPR quantisation for lat/lon fields.
const CPR_SCALE: f64 = 131_072.0;
/// Airborne CPR zone count.
const NZ: f64 = 15.0;

/// Path to the real `readsb` binary; overridable for tests that pin a specific build.
fn readsb_path() -> String {
    std::env::var("HK_READSB").unwrap_or_else(|_| "readsb".into())
}

/// Mode S CRC-24 remainder over the whole frame (header + ME + PI). Zero iff the frame's parity
/// field is consistent with its content (docs/04; the generator matches `py/hkpy/synth/adsb.py`).
fn crc24_remainder(frame: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in frame {
        crc ^= (byte as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= CRC24_GENERATOR;
            }
        }
    }
    crc & 0x00FF_FFFF
}

/// One even or odd CPR-coded airborne position report for one ICAO address.
#[derive(Clone, Copy)]
struct CprFrame {
    lat_cpr: f64,
    lon_cpr: f64,
}

/// `cpr_nl` (the number of longitude zones at a latitude): `py/hkpy/synth/adsb.py::cpr_nl`.
fn cpr_nl(lat: f64) -> i32 {
    if lat == 0.0 {
        return 59;
    }
    if lat.abs() == 87.0 {
        return 2;
    }
    if lat.abs() > 87.0 {
        return 1;
    }
    let a = 1.0 - (std::f64::consts::PI / (2.0 * NZ)).cos();
    let b = (std::f64::consts::PI / 180.0 * lat.abs()).cos().powi(2);
    (2.0 * std::f64::consts::PI / (1.0 - a / b).acos()).floor() as i32
}

/// Globally unambiguous airborne CPR position decode from one even and one odd frame (the
/// standard ADS-B algorithm; inverse of `py/hkpy/synth/adsb.py::cpr_encode`). `None` when the two
/// frames straddle a latitude-zone boundary (`NL` disagrees) and must be discarded.
fn global_position(even: CprFrame, odd: CprFrame, most_recent_odd: bool) -> Option<(f64, f64)> {
    let d_lat_even = 360.0 / (4.0 * NZ);
    let d_lat_odd = 360.0 / (4.0 * NZ - 1.0);
    let rem = |a: f64, m: f64| (a % m + m) % m;
    let j = (59.0 * even.lat_cpr - 60.0 * odd.lat_cpr + 0.5).floor();
    let mut lat_even = d_lat_even * (rem(j, 60.0) + even.lat_cpr);
    let mut lat_odd = d_lat_odd * (rem(j, 59.0) + odd.lat_cpr);
    if lat_even >= 270.0 {
        lat_even -= 360.0;
    }
    if lat_odd >= 270.0 {
        lat_odd -= 360.0;
    }
    let (nl_even, nl_odd) = (cpr_nl(lat_even), cpr_nl(lat_odd));
    if nl_even != nl_odd {
        return None;
    }
    let lat = if most_recent_odd { lat_odd } else { lat_even };
    let ni = (nl_even - i32::from(most_recent_odd)).max(1);
    let d_lon = 360.0 / f64::from(ni);
    let m =
        (even.lon_cpr * f64::from(nl_even - 1) - odd.lon_cpr * f64::from(nl_even) + 0.5).floor();
    let lon_cpr = if most_recent_odd {
        odd.lon_cpr
    } else {
        even.lon_cpr
    };
    let mut lon = d_lon * (rem(m, f64::from(ni)) + lon_cpr);
    if lon > 180.0 {
        lon -= 360.0;
    }
    Some((lat, lon))
}

/// Per-ICAO CPR pairing state (last even/odd airborne position reports).
#[derive(Default)]
struct CprCache {
    even: Option<CprFrame>,
    odd: Option<CprFrame>,
}

/// Reads a big-endian bitfield `[start, start+len)` (MSB-first, 0-indexed) out of an ME payload.
fn bits(me: &[u8], start: u32, len: u32) -> u64 {
    let mut v: u64 = 0;
    for i in start..start + len {
        let byte = me[(i / 8) as usize];
        let bit = (byte >> (7 - (i % 8))) & 1;
        v = (v << 1) | u64::from(bit);
    }
    v
}

/// Decodes the 12-bit Mode S altitude code (Q-bit set, 25 ft increments; inverse of
/// `py/hkpy/synth/adsb.py::encode_altitude`). `None` for Gillham-coded (Q-bit clear) altitudes,
/// which this wrapper does not decode.
fn decode_altitude(code: u32) -> Option<f64> {
    if code & 0x10 == 0 {
        return None;
    }
    let n = ((code >> 5) << 4) | (code & 0xF);
    Some(f64::from(n) * 25.0 - 1000.0)
}

/// One parsed and CRC-verified Mode S frame.
struct Frame {
    icao: String,
    df: u8,
    metadata: Map<String, Value>,
    identity_ok: bool,
}

/// Parses one `*hex;` AVR raw line into a CRC-verified frame, updating `cpr` for airborne
/// position pairing. `None` for anything that fails the CRC check, is too short, or is a downlink
/// format this wrapper does not decode (its ICAO is folded into the parity field).
fn parse_avr_line(line: &str, cpr: &mut HashMap<String, CprCache>) -> Option<Frame> {
    let hex = line.trim().strip_prefix('*')?.strip_suffix(';')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex[i..i + 2], 16).ok()?);
    }
    if bytes.len() != 7 && bytes.len() != 14 {
        return None; // not a short (DF11) or long (DF17) Mode S frame
    }
    if crc24_remainder(&bytes) != 0 {
        return None; // corrupt line: never emit an unverified decode
    }
    let df = bytes[0] >> 3;
    let icao = format!("{:02x}{:02x}{:02x}", bytes[1], bytes[2], bytes[3]);
    let mut metadata = Map::new();
    metadata.insert("icao".into(), json!(icao));
    metadata.insert("df".into(), json!(df));

    if df == 11 {
        // All-call reply: ICAO only, no ME field.
        return Some(Frame {
            icao,
            df,
            metadata,
            identity_ok: true,
        });
    }
    if df != 17 || bytes.len() != 14 {
        return None;
    }
    let me = &bytes[4..11];
    let tc = me[0] >> 3;
    metadata.insert("tc".into(), json!(tc));
    match tc {
        1..=4 => {
            let mut callsign = String::with_capacity(8);
            for i in 0..8 {
                let c = bits(me, 8 + i * 6, 6) as usize;
                callsign.push(CHARSET[c] as char);
            }
            metadata.insert("callsign".into(), json!(callsign.trim_end().to_owned()));
        }
        9..=18 => {
            let alt_code = bits(me, 8, 12) as u32;
            let odd = bits(me, 21, 1) == 1;
            let lat_cpr = bits(me, 22, 17) as f64 / CPR_SCALE;
            let lon_cpr = bits(me, 39, 17) as f64 / CPR_SCALE;
            if let Some(alt_ft) = decode_altitude(alt_code) {
                metadata.insert("alt".into(), json!(alt_ft));
            }
            metadata.insert("cpr_format".into(), json!(if odd { "odd" } else { "even" }));
            let entry = cpr.entry(icao.clone()).or_default();
            let frame = CprFrame { lat_cpr, lon_cpr };
            if odd {
                entry.odd = Some(frame);
            } else {
                entry.even = Some(frame);
            }
            if let (Some(even), Some(odd_frame)) = (entry.even, entry.odd)
                && let Some((lat, lon)) = global_position(even, odd_frame, odd)
            {
                metadata.insert("lat".into(), json!(lat));
                metadata.insert("lon".into(), json!(lon));
            }
        }
        19 => {
            let subtype = bits(me, 5, 3);
            if subtype == 1 || subtype == 2 {
                let ew_sign = bits(me, 13, 1) == 1;
                let ew_field = bits(me, 14, 10);
                let ns_sign = bits(me, 24, 1) == 1;
                let ns_field = bits(me, 25, 10);
                let vr_sign = bits(me, 36, 1) == 1;
                let vr_field = bits(me, 37, 9);
                if ew_field > 0 && ns_field > 0 {
                    let ew = (ew_field as f64 - 1.0) * if ew_sign { -1.0 } else { 1.0 };
                    let ns = (ns_field as f64 - 1.0) * if ns_sign { -1.0 } else { 1.0 };
                    let gs = ew.hypot(ns);
                    let track = (ew.atan2(ns).to_degrees() + 360.0) % 360.0;
                    metadata.insert("gs".into(), json!(gs));
                    metadata.insert("track".into(), json!(track));
                    metadata.insert("ew_velocity_kt".into(), json!(ew));
                    metadata.insert("ns_velocity_kt".into(), json!(ns));
                }
                if vr_field > 0 {
                    let vr = (vr_field as f64 - 1.0) * 64.0 * if vr_sign { -1.0 } else { 1.0 };
                    metadata.insert("vertical_rate_fpm".into(), json!(vr));
                }
            }
        }
        _ => {}
    }
    Some(Frame {
        icao,
        df,
        metadata,
        identity_ok: true,
    })
}

/// Builds one NDJSON `decode` line (docs/stream-contract.md §9.3) for a verified frame.
fn decode_line(frame: &Frame, sample_index: u64) -> String {
    let identity = frame
        .identity_ok
        .then(|| json!({"scheme": "adsb-icao", "value": frame.icao}));
    let mut obj = json!({
        "type": "decode",
        "sample_index": sample_index,
        "frame_model": format!("adsb-df{}", frame.df),
        "crc_status": "valid",
        "metadata": Value::Object(frame.metadata.clone()),
    });
    if let Some(id) = identity {
        obj["identity"] = id;
    }
    obj.to_string()
}

/// Copies readsb's stderr into our own, tagged, so it lands in the host's log ring.
fn pump_stderr(stderr: impl Read) {
    let mut r = BufReader::new(stderr);
    let mut line = String::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        eprint!("readsb: {line}");
        line.clear();
    }
}

/// Parses readsb's `--raw` stdout into NDJSON decode lines on our own stdout.
fn pump_stdout(stdout: impl Read, last_sample_index: &AtomicU64) {
    let mut r = BufReader::new(stdout);
    let mut out = io::stdout().lock();
    let mut line = String::new();
    let mut cpr: HashMap<String, CprCache> = HashMap::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        if let Some(frame) = parse_avr_line(&line, &mut cpr) {
            let idx = last_sample_index.load(Ordering::Relaxed);
            if writeln!(out, "{}", decode_line(&frame, idx)).is_err() || out.flush().is_err() {
                break; // our own stdout closed (host detaching us); nothing more to do
            }
        }
        line.clear();
    }
}

fn spawn_readsb(extra_args: &[String]) -> io::Result<Child> {
    Command::new(readsb_path())
        .args([
            "--device-type",
            "ifile",
            "--ifile",
            "-",
            "--iformat",
            "UC8",
            "--no-interactive",
            "--raw",
            "--no-fix",
        ])
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

fn main() {
    let extra_args: Vec<String> = std::env::args().skip(1).collect();

    let mut reader = StreamReader::new(io::stdin().lock());
    let header = match reader.read_header() {
        Ok(h) => h.clone(),
        Err(e) => {
            eprintln!("hk-plugin-readsb: bad input header: {e}");
            exit(3);
        }
    };
    if header.datatype.as_deref() != Some("ci8") {
        eprintln!(
            "hk-plugin-readsb: header datatype {:?}, expected ci8",
            header.datatype
        );
        exit(4);
    }

    let mut child = match spawn_readsb(&extra_args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hk-plugin-readsb: spawning {} failed: {e}", readsb_path());
            exit(5);
        }
    };
    eprintln!("hk-plugin-readsb: readsb pid {}", child.id());

    let mut readsb_stdin = child.stdin.take().expect("piped stdin");
    let readsb_stdout = child.stdout.take().expect("piped stdout");
    let readsb_stderr = child.stderr.take().expect("piped stderr");

    let last_sample_index = Arc::new(AtomicU64::new(0));
    let idx_for_out = Arc::clone(&last_sample_index);
    let out_thread = thread::spawn(move || pump_stdout(readsb_stdout, &idx_for_out));
    let err_thread = thread::spawn(move || pump_stderr(readsb_stderr));

    // Feed loop: convert ci8 -> uc8 (offset binary) and forward to readsb's stdin. Never blocks
    // capture: this process's own stdin is the bounded `DecoderFeed` ring (host.rs), so a slow
    // readsb backs up there, not here.
    let mut dropped_seen = 0u64;
    loop {
        match reader.next_record() {
            Ok(Some(Record::Binary(b))) => {
                let converted: Vec<u8> = b.payload.iter().map(|byte| byte ^ 0x80).collect();
                if readsb_stdin.write_all(&converted).is_err() {
                    eprintln!("hk-plugin-readsb: readsb's stdin closed; it likely died");
                    break;
                }
                // ci8 is 2 bytes/sample (1 I + 1 Q byte); record the sample just past this chunk
                // as the "most recently fed" anchor for the next messages readsb reports.
                let n_samples = b.header.payload_len as u64 / 2;
                last_sample_index.store(b.header.sample_index + n_samples, Ordering::Relaxed);
            }
            Ok(Some(Record::Dropped(d))) => dropped_seen += d.count,
            Ok(Some(_)) => {}
            Ok(None) => break, // our own stdin at EOF: the host is detaching us
            Err(e) => {
                eprintln!("hk-plugin-readsb: input error: {e}");
                break;
            }
        }
    }
    if dropped_seen > 0 {
        eprintln!("hk-plugin-readsb: {dropped_seen} input records were dropped upstream");
    }
    drop(readsb_stdin); // EOF to readsb: let it finish decoding what it already has and exit

    let status = child.wait();
    let _ = out_thread.join();
    let _ = err_thread.join();
    match status {
        // readsb always reports "Abnormal exit" and a non-zero, non-signalled status when it
        // finishes reading its ifile to EOF (verified against readsb 3.16.16); that is normal
        // completion here, not a crash.
        Ok(s) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = s.signal() {
                    eprintln!("hk-plugin-readsb: readsb killed by signal {sig}");
                    exit(101);
                }
            }
            exit(0);
        }
        Err(e) => {
            eprintln!("hk-plugin-readsb: wait failed: {e}");
            exit(102);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hex-encodes bytes (lower-case), for building `*hex;` AVR test lines without a new
    /// dependency.
    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A synthetic DF17 frame, CRC-24 appended (`py/hkpy/synth/adsb.py::df17`).
    fn df17(icao: u32, me: &[u8; 7]) -> Vec<u8> {
        let mut head = vec![(17 << 3) | 5u8];
        head.extend_from_slice(&icao.to_be_bytes()[1..]);
        head.extend_from_slice(me);
        assert_eq!(head.len(), 11);
        // `crc24_remainder` run over just the 11-byte head *is* `py/hkpy/synth/adsb.py::crc24`:
        // both apply the same generator-polynomial LFSR to the same bytes.
        let crc = crc24_remainder(&head);
        head.push((crc >> 16) as u8);
        head.push((crc >> 8) as u8);
        head.push(crc as u8);
        head
    }

    fn me_identification(callsign: &str, category: u8) -> [u8; 7] {
        let mut bitbuf: u64 = 0;
        let mut nbits = 0u32;
        let mut push = |v: u64, n: u32| {
            bitbuf = (bitbuf << n) | v;
            nbits += n;
        };
        push(4, 5);
        push(category as u64, 3);
        let padded = format!("{callsign:<8}");
        for ch in padded.chars().take(8) {
            let idx = CHARSET.iter().position(|&c| c == ch as u8).unwrap_or(32);
            push(idx as u64, 6);
        }
        assert_eq!(nbits, 56);
        bitbuf.to_be_bytes()[1..].try_into().unwrap()
    }

    #[test]
    fn crc_zero_for_a_valid_frame_and_nonzero_for_a_flipped_bit() {
        let me = me_identification("HKRF01", 0);
        let good = df17(0xa1b2c3, &me);
        assert_eq!(crc24_remainder(&good), 0);
        let mut bad = good.clone();
        bad[5] ^= 0x01;
        assert_ne!(crc24_remainder(&bad), 0);
    }

    #[test]
    fn identification_squitter_decodes_icao_and_callsign() {
        let me = me_identification("HKRF01", 0);
        let bytes = df17(0xa1b2c3, &me);
        let line = format!("*{};", to_hex(&bytes));
        let mut cpr = HashMap::new();
        let frame = parse_avr_line(&line, &mut cpr).expect("valid frame");
        assert_eq!(frame.icao, "a1b2c3");
        assert_eq!(frame.df, 17);
        assert_eq!(frame.metadata["tc"], 4);
        assert_eq!(frame.metadata["callsign"], "HKRF01");
        let json = decode_line(&frame, 12345);
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "decode");
        assert_eq!(v["sample_index"], 12345);
        assert_eq!(v["crc_status"], "valid");
        assert_eq!(v["identity"]["scheme"], "adsb-icao");
        assert_eq!(v["identity"]["value"], "a1b2c3");
    }

    #[test]
    fn corrupted_line_is_never_emitted() {
        let me = me_identification("HKRF01", 0);
        let mut bytes = df17(0xa1b2c3, &me);
        bytes[6] ^= 0xFF;
        let line = format!("*{};", to_hex(&bytes));
        let mut cpr = HashMap::new();
        assert!(parse_avr_line(&line, &mut cpr).is_none());
    }

    #[test]
    fn altitude_round_trips_through_the_synthetic_encoder() {
        // py/hkpy/synth/adsb.py::encode_altitude(alt_ft) inverse.
        for alt_ft in [3000.0_f64, 10025.0, 39000.0] {
            let n = ((alt_ft + 1000.0) / 25.0).round() as u32;
            let code = ((n >> 4) << 5) | 0x10 | (n & 0xF);
            assert_eq!(decode_altitude(code), Some(alt_ft));
        }
    }

    #[test]
    fn paired_even_odd_cpr_frames_resolve_a_position_near_the_truth() {
        // Mirrors py/hkpy/synth/adsb.py::cpr_encode for a known lat/lon.
        fn cpr_encode(lat: f64, lon: f64, odd: bool) -> (u32, u32) {
            let i = if odd { 1.0 } else { 0.0 };
            let dlat = 360.0 / (4.0 * NZ - i);
            let yz = (CPR_SCALE * ((lat.rem_euclid(dlat)) / dlat) + 0.5).floor();
            let rlat = dlat * (yz / CPR_SCALE + (lat / dlat).floor());
            let dlon = 360.0 / f64::from(cpr_nl(rlat) - i as i32).max(1.0);
            let xz = (CPR_SCALE * ((lon.rem_euclid(dlon)) / dlon) + 0.5).floor();
            (yz as u32 & 0x1FFFF, xz as u32 & 0x1FFFF)
        }
        let (lat, lon) = (52.4, -0.5);
        let (ylat_e, xlon_e) = cpr_encode(lat, lon, false);
        let (ylat_o, xlon_o) = cpr_encode(lat, lon, true);
        let even = CprFrame {
            lat_cpr: f64::from(ylat_e) / CPR_SCALE,
            lon_cpr: f64::from(xlon_e) / CPR_SCALE,
        };
        let odd = CprFrame {
            lat_cpr: f64::from(ylat_o) / CPR_SCALE,
            lon_cpr: f64::from(xlon_o) / CPR_SCALE,
        };
        let (dec_lat, dec_lon) = global_position(even, odd, true).expect("resolves");
        assert!((dec_lat - lat).abs() < 1e-3, "{dec_lat} vs {lat}");
        assert!((dec_lon - lon).abs() < 1e-3, "{dec_lon} vs {lon}");
    }
}
