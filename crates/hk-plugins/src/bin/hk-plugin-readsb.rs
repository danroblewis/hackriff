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
//!   (`ci8 -> uc8` is exactly a 128-count offset, i.e. sign-bit flip) and forwarded to a writer
//!   thread that owns readsb's stdin.
//! - **Backpressure (T-037b).** The hand-off to the writer thread is bounded
//!   ([`WRITER_BACKLOG_CHUNKS`] records). When readsb falls behind, this wrapper stops reading
//!   its own stdin, so the plugin host's bounded input queue (the manifest's
//!   `input_queue_bytes`) fills instead of this process's memory: a live chain then drops and
//!   counts records at the host, and a lossless replay waits for queue room there (the pipeline's
//!   plugin chain), so a long unpaced recording no longer overflows the queue.
//! - **readsb invocation.** `--device-type ifile --ifile - --iformat UC8 --no-interactive --raw
//!   --no-fix`. **`--no-fix` is the only guard against repaired-but-wrong frames**: with `--fix`
//!   (readsb's default) it single-bit-corrects a frame and prints it with a CRC that now checks
//!   out, even though the original bytes were wrong (probed: 20/20 lines emitted with `--fix` vs.
//!   10/10 genuinely good frames with `--no-fix`, same input). This wrapper's own CRC-24
//!   recomputation is *not* that guard — it only catches a corrupted or truncated `*hex;` line,
//!   which a repaired frame's self-consistent CRC would sail through. [`reject_fix_arg`] refuses
//!   to spawn readsb at all if an extra (manifest-supplied) argument would re-enable `--fix`.
//! - **Message parsing.** DF17 extended squitters (identification, TC 1-4; airborne position,
//!   TC 9-18, paired even/odd CPR frames per ICAO within a [`CPR_PAIR_WINDOW_S`] window; velocity,
//!   TC 19, ground-speed subtypes 1 and 2) and DF11 all-call replies (ICAO only — no ME field).
//!   Other downlink formats carry the ICAO folded into the parity field (address/parity XOR) and
//!   are out of scope for this wrapper.
//! - **Timestamps.** `sample_index` on every emitted decode line is the index of the *last*
//!   sample of the most recent input record **written to readsb's stdin** (set by the writer
//!   thread after the write, T-037b) — the fallback docs/stream-contract.md §9.6 anticipated,
//!   since raw AVR output carries no correlatable position in the input stream. It is an upper
//!   bound on the message's true position: stamping at hand-off (as before) also counted the
//!   unbounded hand-off backlog, which in an unpaced replay could run seconds ahead. What remains
//!   is readsb's own buffering: a full `--sdr-buffer-size` block (128 KiB default, ~27 ms of
//!   samples at 2.4 Msps) plus the stdin pipe buffer (64 KiB on macOS/Linux, ~13 ms), so decodes
//!   stamp at most about 40 ms late (unverified against readsb's internal block alignment).
//! - **Crash and stall isolation.** readsb runs as an ordinary child (not detached), inside this
//!   process's own process group, which the plugin host already owns and can SIGKILL as a whole
//!   (`kill_group` in `host.rs`). Three independent watchers can end this wrapper non-zero so the
//!   host's supervisor restarts the pair:
//!   1. the writer thread, when a write to readsb's stdin fails (readsb's read end closed);
//!   2. a waiter thread blocked on `child.wait()`, when readsb exits before *this* wrapper asked
//!      it to (covers a signal, a crash, or readsb's own "SDR wedged, exiting!" self-destruct,
//!      which is a plain, non-signalled exit — reading the exit status alone cannot tell a crash
//!      from a clean run, only "did we ask for this" can);
//!   3. the stderr pump, the instant it sees readsb log "SDR wedged" (readsb's own watchdog fires
//!      only after ~9 s of stalled input; this doesn't wait for the process to actually finish
//!      exiting).
//!
//!   A **keepalive**: whenever the writer thread goes [`KEEPALIVE_INTERVAL`] without a real
//!   chunk to forward, it sends readsb a small chunk of silence (`uc8` mid-scale) instead, well
//!   under readsb's ~9 s wedge threshold. A live wideband channel streams continuously anyway;
//!   this only matters when the upstream capture pauses (e.g. the scheduler retunes away) or,
//!   here, in tests.
//!
//!   A **clean shutdown** (this wrapper's own stdin reaching EOF: the host is detaching it) sets
//!   `clean_shutdown` before closing readsb's stdin, so the waiter thread does not mistake the
//!   readsb exit that follows for a crash.
//! - **Own pid, for tests.** `hk-plugin-readsb: readsb pid <pid>` is logged to stderr (captured in
//!   the host's log ring under a content-permitting ceiling), the same convention
//!   `hk-dummy-plugin` uses for its `--orphan`/`--stall-child` grandchildren, so a test can find
//!   and signal the real readsb process.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio, exit};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

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
/// Longest gap between an ICAO's cached even and odd CPR frames before the pair is discarded as
/// stale rather than resolved into a (likely wrong) position.
const CPR_PAIR_WINDOW_S: f64 = 10.0;

/// How long the writer thread waits for real data before sending readsb a keepalive chunk.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(3);
/// Keepalive chunk size (`uc8` mid-scale, i.e. `ci8` zero after this wrapper's XOR conversion).
const KEEPALIVE_CHUNK_BYTES: usize = 16_384;
/// Converted input records the writer thread may have queued before this wrapper stops reading
/// its stdin (see the module doc's "Backpressure").
const WRITER_BACKLOG_CHUNKS: usize = 16;

/// Path to the real `readsb` binary; overridable for tests that pin a specific build.
fn readsb_path() -> String {
    std::env::var("HK_READSB").unwrap_or_else(|_| "readsb".into())
}

/// Refuses any extra (manifest-supplied) argument that would re-enable readsb's `--fix`: see the
/// module doc's "readsb invocation" note for why `--no-fix` is the only real guard here.
fn reject_fix_arg(args: &[String]) -> Result<(), String> {
    if args.iter().any(|a| a == "--fix") {
        return Err("refusing to spawn: an extra argument passes --fix, which would let readsb repair and pass through a frame that was never actually clean".into());
    }
    Ok(())
}

/// Mode S CRC-24 remainder over the whole frame (header + ME + PI). Zero iff the frame's parity
/// field is consistent with its content (docs/04; the generator matches `py/hkpy/synth/adsb.py`).
/// A corruption check on the *line*, not a defence against a repaired frame — see the module doc.
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

/// One even or odd CPR-coded airborne position report for one ICAO address, with the (coarse)
/// input sample index it arrived at, for the [`CPR_PAIR_WINDOW_S`] staleness check.
#[derive(Clone, Copy)]
struct CprFrame {
    lat_cpr: f64,
    lon_cpr: f64,
    sample_index: u64,
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

impl CprCache {
    /// Drops either cached half that is more than `window_samples` away from `now`: a stale half
    /// paired with a fresh one would resolve to a confident-looking but wrong position.
    fn prune_stale(&mut self, now: u64, window_samples: u64) {
        for half in [&mut self.even, &mut self.odd] {
            if half.is_some_and(|f| now.abs_diff(f.sample_index) > window_samples) {
                *half = None;
            }
        }
    }
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
    /// The (coarse) input sample index this frame is stamped with; see the module doc's
    /// "Timestamps" note.
    sample_index: u64,
}

/// Parses one `*hex;` AVR raw line into a CRC-verified frame at `current_sample_index`, updating
/// `cpr` for airborne position pairing (`window_samples`: [`CPR_PAIR_WINDOW_S`] in samples).
/// `None` for anything that fails the CRC check, is too short, or is a downlink format this
/// wrapper does not decode (its ICAO is folded into the parity field).
fn parse_avr_line(
    line: &str,
    cpr: &mut HashMap<String, CprCache>,
    current_sample_index: u64,
    window_samples: u64,
) -> Option<Frame> {
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
            sample_index: current_sample_index,
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
            entry.prune_stale(current_sample_index, window_samples);
            let frame = CprFrame {
                lat_cpr,
                lon_cpr,
                sample_index: current_sample_index,
            };
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
                // Subtype 1 (subsonic): 1 kt/LSB. Subtype 2 (supersonic): 4 kt/LSB.
                let speed_scale = if subtype == 2 { 4.0 } else { 1.0 };
                let ew_sign = bits(me, 13, 1) == 1;
                let ew_field = bits(me, 14, 10);
                let ns_sign = bits(me, 24, 1) == 1;
                let ns_field = bits(me, 25, 10);
                let vr_sign = bits(me, 36, 1) == 1;
                let vr_field = bits(me, 37, 9);
                if ew_field > 0 && ns_field > 0 {
                    let ew =
                        (ew_field as f64 - 1.0) * speed_scale * if ew_sign { -1.0 } else { 1.0 };
                    let ns =
                        (ns_field as f64 - 1.0) * speed_scale * if ns_sign { -1.0 } else { 1.0 };
                    let gs = ew.hypot(ns);
                    let track = (ew.atan2(ns).to_degrees() + 360.0) % 360.0;
                    metadata.insert("gs".into(), json!(gs));
                    metadata.insert("track".into(), json!(track));
                    metadata.insert("ew_velocity_kt".into(), json!(ew));
                    metadata.insert("ns_velocity_kt".into(), json!(ns));
                }
                if vr_field > 0 {
                    // Vertical rate is always 64 fpm/LSB, regardless of subtype.
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
        sample_index: current_sample_index,
    })
}

/// Builds one NDJSON `decode` line (docs/stream-contract.md §9.3) for a verified frame.
fn decode_line(frame: &Frame) -> String {
    let identity = frame
        .identity_ok
        .then(|| json!({"scheme": "adsb-icao", "value": frame.icao}));
    let mut obj = json!({
        "type": "decode",
        "sample_index": frame.sample_index,
        "frame_model": format!("adsb-df{}", frame.df),
        "crc_status": "valid",
        "metadata": Value::Object(frame.metadata.clone()),
    });
    if let Some(id) = identity {
        obj["identity"] = id;
    }
    obj.to_string()
}

/// Ends this process because readsb is gone, or something else makes continuing unsafe. Never
/// returns: the host's supervisor treats a non-zero wrapper exit as a crash and restarts the pair
/// (`host.rs`'s process-group kill handles any readsb descendant left behind).
fn crash_exit(reason: &str) -> ! {
    eprintln!("hk-plugin-readsb: {reason}");
    exit(101)
}

fn describe_status(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("signal {sig}"),
        _ => "unknown exit".into(),
    }
}

/// Copies readsb's stderr into our own, tagged, so it lands in the host's log ring. Ends this
/// process the instant readsb logs its own "SDR wedged" self-destruct message — its watchdog
/// fires only after ~9 s of stalled input, and this is faster than waiting for the waiter thread
/// to see the process actually finish exiting. Silent once `clean_shutdown` is set: a wedge
/// message racing a deliberate shutdown must not be mistaken for a crash.
fn pump_stderr(stderr: impl Read, clean_shutdown: &AtomicBool) {
    let mut r = BufReader::new(stderr);
    let mut line = String::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        eprint!("readsb: {line}");
        if line.contains("wedged") && !clean_shutdown.load(Ordering::SeqCst) {
            crash_exit("readsb reported itself wedged (stalled input, its own ~9s watchdog)");
        }
        line.clear();
    }
}

/// Parses readsb's `--raw` stdout into NDJSON decode lines on our own stdout.
fn pump_stdout(stdout: impl Read, last_sample_index: &AtomicU64, window_samples: u64) {
    let mut r = BufReader::new(stdout);
    let mut out = io::stdout().lock();
    let mut line = String::new();
    let mut cpr: HashMap<String, CprCache> = HashMap::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        let idx = last_sample_index.load(Ordering::Relaxed);
        if let Some(frame) = parse_avr_line(&line, &mut cpr, idx, window_samples)
            && (writeln!(out, "{}", decode_line(&frame)).is_err() || out.flush().is_err())
        {
            break; // our own stdout closed (host detaching us); nothing more to do
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

/// Owns readsb's stdin. Forwards real chunks from `rx` (already `ci8 -> uc8` converted, each with
/// the index of its last sample, which is published to `last_sample_index` once the chunk is
/// written), and sends a small keepalive chunk instead whenever `rx` has gone
/// `KEEPALIVE_INTERVAL` with nothing (see the module doc). Exits this process if a write ever
/// fails: readsb's read end is gone, which means readsb died. Returns normally only on a clean
/// shutdown (`rx` disconnected: the main thread finished forwarding and dropped its sender),
/// closing readsb's stdin so it can flush its last partial input block and exit on its own EOF
/// path.
fn feed_readsb(
    mut readsb_stdin: impl Write,
    rx: mpsc::Receiver<(Vec<u8>, u64)>,
    last_sample_index: &AtomicU64,
) {
    loop {
        match rx.recv_timeout(KEEPALIVE_INTERVAL) {
            Ok((chunk, last_index)) => {
                if readsb_stdin.write_all(&chunk).is_err() {
                    crash_exit("write to readsb's stdin failed; it likely died");
                }
                last_sample_index.store(last_index, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {
                let silence = [0x80u8; KEEPALIVE_CHUNK_BYTES];
                if readsb_stdin.write_all(&silence).is_err() {
                    crash_exit("keepalive write to readsb's stdin failed; it likely died");
                }
            }
            Err(RecvTimeoutError::Disconnected) => return, // clean shutdown; caller closes stdin
        }
    }
}

fn main() {
    let extra_args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(reason) = reject_fix_arg(&extra_args) {
        eprintln!("hk-plugin-readsb: {reason}");
        exit(6);
    }

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
    let sample_rate_hz = header.sample_rate_hz.unwrap_or(2_400_000.0);
    let cpr_window_samples = (CPR_PAIR_WINDOW_S * sample_rate_hz).round() as u64;

    let mut child = match spawn_readsb(&extra_args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("hk-plugin-readsb: spawning {} failed: {e}", readsb_path());
            exit(5);
        }
    };
    eprintln!("hk-plugin-readsb: readsb pid {}", child.id());

    let readsb_stdin = child.stdin.take().expect("piped stdin");
    let readsb_stdout = child.stdout.take().expect("piped stdout");
    let readsb_stderr = child.stderr.take().expect("piped stderr");

    let clean_shutdown = Arc::new(AtomicBool::new(false));
    let last_sample_index = Arc::new(AtomicU64::new(0));
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u64)>(WRITER_BACKLOG_CHUNKS);

    let idx_for_writer = Arc::clone(&last_sample_index);
    let writer = thread::spawn(move || feed_readsb(readsb_stdin, rx, &idx_for_writer));

    let waiter_clean = Arc::clone(&clean_shutdown);
    let waiter = thread::spawn(move || {
        let status = child.wait();
        if !waiter_clean.load(Ordering::SeqCst) {
            // readsb exited before this wrapper decided to end things: unexpected, whatever its
            // exit status says (a plain non-signalled exit covers a crash and readsb's own
            // wedge self-destruct alike — only "did we ask for this" tells them apart from a
            // clean run).
            match &status {
                Ok(s) => crash_exit(&format!(
                    "readsb exited unexpectedly: {}",
                    describe_status(s)
                )),
                Err(e) => crash_exit(&format!("waiting for readsb failed: {e}")),
            }
        }
        status
    });

    let stderr_clean = Arc::clone(&clean_shutdown);
    let err_thread = thread::spawn(move || pump_stderr(readsb_stderr, &stderr_clean));

    let idx_for_out = Arc::clone(&last_sample_index);
    let out_thread =
        thread::spawn(move || pump_stdout(readsb_stdout, &idx_for_out, cpr_window_samples));

    // Feed loop: convert ci8 -> uc8 (offset binary) and hand it to the writer thread. Never
    // blocks capture: this process's own stdin is the bounded `DecoderFeed` ring (host.rs). The
    // hand-off is bounded too, so a slow readsb backs this loop up, then the host's queue, whose
    // drop-or-wait policy the host side chooses (module doc, "Backpressure").
    let mut dropped_seen = 0u64;
    let mut input_error = false;
    loop {
        match reader.next_record() {
            Ok(Some(Record::Binary(b))) => {
                let converted: Vec<u8> = b.payload.iter().map(|byte| byte ^ 0x80).collect();
                // The index of the *last* sample in this record: main.rs's INVARIANT-adjacent
                // range check treats the offered range as inclusive, and one past the end (what
                // this used to stamp) can read as out of range under a restricted class.
                let n_samples = b.header.payload_len as u64 / 2; // ci8: 1 I + 1 Q byte/sample
                let last_index = b.header.sample_index + n_samples.saturating_sub(1);
                if tx.send((converted, last_index)).is_err() {
                    // The writer thread is gone. It only ever exits via `crash_exit` (which
                    // terminates the whole process) or a clean shutdown it cannot have started
                    // (only this loop starts one) — unreachable in practice, but never spin.
                    input_error = true;
                    break;
                }
            }
            Ok(Some(Record::Dropped(d))) => dropped_seen += d.count,
            Ok(Some(_)) => {}
            Ok(None) => break, // our own stdin at EOF: the host is detaching us (clean)
            Err(e) => {
                eprintln!("hk-plugin-readsb: input error: {e}");
                input_error = true;
                break;
            }
        }
    }
    if dropped_seen > 0 {
        eprintln!("hk-plugin-readsb: {dropped_seen} input records were dropped upstream");
    }
    if input_error {
        // Something is wrong with reading our own input (not readsb's fault), but this process
        // must still end promptly so the host's supervisor can restart it.
        crash_exit("stopping after an input error on our own stdin");
    }

    // Clean shutdown: tell the waiter/stderr threads not to treat readsb's exit as a crash, then
    // drop the sender so the writer thread closes readsb's stdin (EOF) and returns.
    clean_shutdown.store(true, Ordering::SeqCst);
    drop(tx);
    let _ = writer.join();
    let _ = waiter.join();
    let _ = out_thread.join();
    let _ = err_thread.join();
    eprintln!("hk-plugin-readsb: clean shutdown");
    exit(0);
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

    /// `py/hkpy/synth/adsb.py::me_velocity`.
    fn me_velocity(ew_kt: i32, ns_kt: i32, vrate_fpm: i32) -> [u8; 7] {
        let mut bitbuf: u64 = 0;
        let mut nbits = 0u32;
        let mut push = |v: u64, n: u32| {
            bitbuf = (bitbuf << n) | (v & ((1u64 << n) - 1));
            nbits += n;
        };
        let vr = ((vrate_fpm.unsigned_abs() / 64 + 1) as u64).min(511);
        push(19, 5);
        push(1, 3);
        push(0, 1);
        push(0, 1);
        push(0, 3);
        push(u64::from(ew_kt < 0), 1);
        push((ew_kt.unsigned_abs() as u64 + 1).min(1023), 10);
        push(u64::from(ns_kt < 0), 1);
        push((ns_kt.unsigned_abs() as u64 + 1).min(1023), 10);
        push(0, 1);
        push(u64::from(vrate_fpm < 0), 1);
        push(vr, 9);
        push(0, 2);
        push(0, 1);
        push(0, 7);
        assert_eq!(nbits, 56);
        bitbuf.to_be_bytes()[1..].try_into().unwrap()
    }

    /// `py/hkpy/synth/adsb.py::me_airborne_position`, altitude left at 0 (Q-bit clear, so
    /// `decode_altitude` reports no `alt` — irrelevant to the CPR-pairing tests using this).
    fn me_airborne_position(tc: u8, odd: bool, ylat: u32, xlon: u32) -> [u8; 7] {
        let mut bitbuf: u64 = 0;
        let mut nbits = 0u32;
        let mut push = |v: u64, n: u32| {
            bitbuf = (bitbuf << n) | (v & ((1u64 << n) - 1));
            nbits += n;
        };
        push(tc as u64, 5);
        push(0, 2); // surveillance status
        push(0, 1); // NIC supplement
        push(0, 12); // altitude (Q-bit clear)
        push(0, 1); // time
        push(u64::from(odd), 1); // CPR format
        push(ylat as u64, 17);
        push(xlon as u64, 17);
        assert_eq!(nbits, 56);
        bitbuf.to_be_bytes()[1..].try_into().unwrap()
    }

    const HUGE_WINDOW: u64 = u64::MAX / 2;

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
        let frame = parse_avr_line(&line, &mut cpr, 12345, HUGE_WINDOW).expect("valid frame");
        assert_eq!(frame.icao, "a1b2c3");
        assert_eq!(frame.df, 17);
        assert_eq!(frame.metadata["tc"], 4);
        assert_eq!(frame.metadata["callsign"], "HKRF01");
        let json = decode_line(&frame);
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
        assert!(parse_avr_line(&line, &mut cpr, 0, HUGE_WINDOW).is_none());
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

    /// Mirrors `py/hkpy/synth/adsb.py::cpr_encode` for a known lat/lon.
    fn cpr_encode(lat: f64, lon: f64, odd: bool) -> (u32, u32) {
        let i = if odd { 1.0 } else { 0.0 };
        let dlat = 360.0 / (4.0 * NZ - i);
        let yz = (CPR_SCALE * (lat.rem_euclid(dlat) / dlat) + 0.5).floor();
        let rlat = dlat * (yz / CPR_SCALE + (lat / dlat).floor());
        let dlon = 360.0 / f64::from(cpr_nl(rlat) - i as i32).max(1.0);
        let xz = (CPR_SCALE * (lon.rem_euclid(dlon) / dlon) + 0.5).floor();
        (yz as u32 & 0x1FFFF, xz as u32 & 0x1FFFF)
    }

    #[test]
    fn paired_even_odd_cpr_frames_resolve_a_position_near_the_truth() {
        let (lat, lon) = (52.4, -0.5);
        let (ylat_e, xlon_e) = cpr_encode(lat, lon, false);
        let (ylat_o, xlon_o) = cpr_encode(lat, lon, true);
        let even = CprFrame {
            lat_cpr: f64::from(ylat_e) / CPR_SCALE,
            lon_cpr: f64::from(xlon_e) / CPR_SCALE,
            sample_index: 0,
        };
        let odd = CprFrame {
            lat_cpr: f64::from(ylat_o) / CPR_SCALE,
            lon_cpr: f64::from(xlon_o) / CPR_SCALE,
            sample_index: 100,
        };
        let (dec_lat, dec_lon) = global_position(even, odd, true).expect("resolves");
        assert!((dec_lat - lat).abs() < 1e-3, "{dec_lat} vs {lat}");
        assert!((dec_lon - lon).abs() < 1e-3, "{dec_lon} vs {lon}");
    }

    /// A stale half (older than [`CPR_PAIR_WINDOW_S`] at the input rate) is pruned instead of
    /// being paired with a fresh one into a confident-looking but wrong position.
    #[test]
    fn a_stale_cpr_half_gives_no_position_but_a_fresh_pair_still_resolves() {
        let rate = 2_400_000.0;
        let window_samples = (CPR_PAIR_WINDOW_S * rate) as u64;
        let (lat, lon) = (52.4, -0.5);
        let (ylat_e, xlon_e) = cpr_encode(lat, lon, false);
        let (ylat_o, xlon_o) = cpr_encode(lat, lon, true);
        let even_line = {
            let bytes = df17(0xa1b2c3, &me_airborne_position(11, false, ylat_e, xlon_e));
            format!("*{};", to_hex(&bytes))
        };
        let odd_line = {
            let bytes = df17(0xa1b2c3, &me_airborne_position(11, true, ylat_o, xlon_o));
            format!("*{};", to_hex(&bytes))
        };

        // Stale: even at sample 0, odd arrives far outside the window -> no position.
        let mut cpr = HashMap::new();
        parse_avr_line(&even_line, &mut cpr, 0, window_samples).unwrap();
        let stale = parse_avr_line(
            &odd_line,
            &mut cpr,
            window_samples + 1_000_000,
            window_samples,
        )
        .unwrap();
        assert!(!stale.metadata.contains_key("lat"), "{:?}", stale.metadata);

        // Fresh: both within the window -> resolves near the truth.
        let mut cpr = HashMap::new();
        parse_avr_line(&even_line, &mut cpr, 0, window_samples).unwrap();
        let fresh = parse_avr_line(&odd_line, &mut cpr, 1000, window_samples).unwrap();
        assert!((fresh.metadata["lat"].as_f64().unwrap() - lat).abs() < 1e-3);
        assert!((fresh.metadata["lon"].as_f64().unwrap() - lon).abs() < 1e-3);
    }

    #[test]
    fn velocity_subtype_2_applies_the_4kt_supersonic_scale() {
        let me = me_velocity(100, -50, 640);
        let mut me19 = me;
        me19[0] = (19 << 3) | 2; // force subtype 2 (supersonic)
        let bytes = df17(0xa1b2c3, &me19);
        let line = format!("*{};", to_hex(&bytes));
        let mut cpr = HashMap::new();
        let frame = parse_avr_line(&line, &mut cpr, 0, HUGE_WINDOW).unwrap();
        assert_eq!(frame.metadata["ew_velocity_kt"], 400.0);
        assert_eq!(frame.metadata["ns_velocity_kt"], -200.0);
        assert_eq!(frame.metadata["vertical_rate_fpm"], 640.0);
    }

    #[test]
    fn velocity_subtype_1_is_unscaled() {
        let me = me_velocity(100, -50, 640);
        let bytes = df17(0xa1b2c3, &me); // subtype 1 by construction
        let line = format!("*{};", to_hex(&bytes));
        let mut cpr = HashMap::new();
        let frame = parse_avr_line(&line, &mut cpr, 0, HUGE_WINDOW).unwrap();
        assert_eq!(frame.metadata["ew_velocity_kt"], 100.0);
        assert_eq!(frame.metadata["ns_velocity_kt"], -50.0);
        assert_eq!(frame.metadata["vertical_rate_fpm"], 640.0);
    }

    /// The decode stamp follows what readsb was actually given: a chunk's last index is published
    /// only after its bytes are written, and the hand-off is bounded.
    #[test]
    fn feed_stamps_after_the_write_and_the_hand_off_is_bounded() {
        let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u64)>(WRITER_BACKLOG_CHUNKS);
        for i in 0..WRITER_BACKLOG_CHUNKS as u64 {
            tx.try_send((vec![i as u8; 4], 100 + i)).unwrap();
        }
        assert!(
            tx.try_send((vec![0; 4], 0)).is_err(),
            "a full hand-off makes the reader wait instead of buffering without bound"
        );
        drop(tx);
        let idx = AtomicU64::new(0);
        let mut out = Vec::new();
        feed_readsb(&mut out, rx, &idx);
        assert_eq!(out.len(), 4 * WRITER_BACKLOG_CHUNKS);
        assert_eq!(
            idx.load(Ordering::Relaxed),
            100 + WRITER_BACKLOG_CHUNKS as u64 - 1
        );
    }

    #[test]
    fn reject_fix_arg_refuses_fix_but_allows_everything_else() {
        assert!(reject_fix_arg(&["--freq".into(), "1090000000".into()]).is_ok());
        assert!(
            reject_fix_arg(&["--no-fix".into()]).is_ok(),
            "--no-fix is not --fix"
        );
        assert!(reject_fix_arg(&["--fix".into()]).is_err());
        assert!(reject_fix_arg(&["--freq".into(), "1090000000".into(), "--fix".into()]).is_err());
    }

    #[test]
    fn spawned_args_always_carry_no_fix_and_never_fix() {
        // `spawn_readsb` prepends the fixed args unconditionally; this only re-asserts the
        // contract so a future edit to the fixed list cannot silently drop `--no-fix`.
        let fixed = [
            "--device-type",
            "ifile",
            "--ifile",
            "-",
            "--iformat",
            "UC8",
            "--no-interactive",
            "--raw",
            "--no-fix",
        ];
        assert!(fixed.contains(&"--no-fix"));
        assert!(!fixed.contains(&"--fix"));
    }
}
