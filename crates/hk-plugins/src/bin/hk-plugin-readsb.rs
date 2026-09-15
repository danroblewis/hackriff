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
//! - **Timestamps (T-072).** `sample_index` on every emitted decode line is the stream index of
//!   the message's preamble start, taken from readsb's own sample clock rather than from when a
//!   line happened to be read. readsb also runs a Beast output to a loopback listener of this
//!   wrapper (`--net --net-connector=127.0.0.1,<port>,beast_out`); every Beast frame carries a
//!   48-bit 12 MHz timestamp counted from the first sample readsb read on stdin. The writer
//!   thread records where each forwarded input record landed in that count (start-up pre-roll and
//!   keepalive silence map to nothing), so a frame's timestamp maps back to the record's stream
//!   index whatever the pipe buffering, readsb's block size or the load. readsb (3.16) stamps a
//!   message [`BEAST_STAMP_OFFSET_S`] after its preamble start (its 136 µs buffer overlap plus the
//!   64 µs Beast reference point; measured exact to the sample on the `adsb_squitter` synth),
//!   which is subtracted.
//!
//!   The `--raw` stdout lines stay the authoritative message list: each is matched to the Beast
//!   frame with the same bytes, waiting up to [`BEAST_MATCH_WAIT`]. A line without a frame (no
//!   connection, a frame readsb did not forward) falls back to the old upper bound, the last
//!   sample of the newest record written to readsb's stdin: readsb's block (~27 ms) plus the pipe
//!   buffer (~13 ms), and T-047 saw up to ~95 ms under load. readsb opens its connector only once
//!   input flows, so the writer first sends [`PREROLL_SAMPLES`] of silence and waits up to
//!   [`BEAST_CONNECT_WAIT`] for the connection before forwarding real samples; a message in the
//!   first milliseconds of a chain would otherwise decode before the connection exists.
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

use std::collections::{HashMap, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, ExitStatus, Stdio, exit};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

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
    let bytes = avr_bytes(line)?;
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

/// readsb's Beast timestamp clock, ticks per second.
const BEAST_TICKS_PER_S: f64 = 12e6;
/// How long after its preamble start readsb stamps a message (see the module doc, "Timestamps").
const BEAST_STAMP_OFFSET_S: f64 = 200e-6;
/// Silence written after readsb's Beast connection is up and before the first real record, so the
/// blocks readsb decodes while it wires its new Beast client up carry no messages (T-223).
/// readsb's `ifile` reader waits for full `--sdr-buffer-size` blocks (128 KiB = 65 536 `uc8`
/// samples), and readsb 3.16 did not forward over Beast a message in its first few blocks even
/// with the connection up (measured on the `adsb_squitter` synth: a 131 072-sample pre-roll lost
/// the first squitter's frame, 600 000 and more lost none). 2²⁰ samples (~0.44 s at 2.4 Msps, a
/// few ms of readsb CPU per chain start) keeps a margin; a missed frame still falls back.
const PREROLL_SAMPLES: u64 = 1 << 20;
/// Longest wait for readsb's Beast connection before real samples are forwarded anyway. readsb
/// runs at the manifest's `nice` and opens its connector from its main loop, which under heavy
/// load took over the old 2 s: the chain's first squitters then decoded before the connection
/// existed and kept only the fallback stamp (T-103). Real samples wait in the host's bounded
/// input queue meanwhile (a lossless replay waits for room; a live chain drops and counts), so a
/// longer wait loses nothing; it stays under the chain's 30 s lossless stall limit.
const BEAST_CONNECT_WAIT: Duration = Duration::from_secs(20);
/// Longest the main thread holds the contract's `ready` line (T-223) waiting for the writer
/// thread's Beast connection and pre-roll. Past it the wrapper reports ready anyway and runs with
/// fallback stamps, rather than leaving the host's chain waiting on it.
const READY_WAIT: Duration = Duration::from_secs(25);
/// Longest wait for the Beast frame of a raw line.
const BEAST_MATCH_WAIT: Duration = Duration::from_millis(250);
/// Unmatched Beast frames kept (frames of downlink formats this wrapper never emits).
const BEAST_MAX_PENDING: usize = 4096;
/// Forwarded records whose position in readsb's sample count is remembered.
const PLACEMENTS_KEPT: usize = 4096;

#[derive(Default)]
struct BeastState {
    connected: bool,
    closed: bool,
    /// `(message bytes, timestamp)`, in readsb's output order.
    frames: VecDeque<(Vec<u8>, u64)>,
}

/// Sample-time stamping state shared by the writer, Beast and stdout threads (module doc,
/// "Timestamps").
struct Timing {
    sample_rate_hz: f64,
    /// Last sample of the newest record written to readsb's stdin: the fallback stamp.
    last_sample_index: AtomicU64,
    /// `(readsb sample count at the record's first sample, its stream index, samples)`.
    placements: Mutex<VecDeque<(u64, u64, u64)>>,
    beast: Mutex<BeastState>,
    beast_changed: Condvar,
}

impl Timing {
    fn new(sample_rate_hz: f64) -> Self {
        Self {
            sample_rate_hz,
            last_sample_index: AtomicU64::new(0),
            placements: Mutex::new(VecDeque::new()),
            beast: Mutex::new(BeastState::default()),
            beast_changed: Condvar::new(),
        }
    }

    fn beast(&self) -> MutexGuard<'_, BeastState> {
        self.beast.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A record of `n` samples starting at stream index `first` was written at readsb sample
    /// count `at`.
    fn placed(&self, at: u64, first: u64, n: u64) {
        let mut p = self
            .placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if p.len() >= PLACEMENTS_KEPT {
            p.pop_front();
        }
        p.push_back((at, first, n));
        drop(p);
        self.last_sample_index
            .store(first + n.saturating_sub(1), Ordering::Relaxed);
    }

    /// The stream index at readsb sample count `at`, when a forwarded record covers it.
    fn index_at(&self, at: u64) -> Option<u64> {
        let p = self
            .placements
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let i = p
            .partition_point(|&(start, _, _)| start <= at)
            .checked_sub(1)?;
        let (start, first, n) = p[i];
        (at < start + n).then(|| first + (at - start))
    }

    /// The stream index of the preamble start a Beast timestamp names.
    fn index_of_timestamp(&self, ticks: u64) -> Option<u64> {
        let at = (ticks as f64 / BEAST_TICKS_PER_S - BEAST_STAMP_OFFSET_S) * self.sample_rate_hz;
        if at.is_nan() || at < 0.0 {
            return None;
        }
        self.index_at(at.round() as u64)
    }

    fn set_connected(&self) {
        self.beast().connected = true;
        self.beast_changed.notify_all();
    }

    fn set_closed(&self) {
        self.beast().closed = true;
        self.beast_changed.notify_all();
    }

    fn push_frame(&self, message: Vec<u8>, ticks: u64) {
        let mut st = self.beast();
        if st.frames.len() >= BEAST_MAX_PENDING {
            st.frames.pop_front();
        }
        st.frames.push_back((message, ticks));
        drop(st);
        self.beast_changed.notify_all();
    }

    /// Waits up to `timeout` for the Beast connection; whether it exists.
    fn wait_connected(&self, timeout: Duration) -> bool {
        let st = self.beast();
        let (st, _) = self
            .beast_changed
            .wait_timeout_while(st, timeout, |s| !s.connected && !s.closed)
            .unwrap_or_else(PoisonError::into_inner);
        st.connected
    }

    /// The stamp of CRC-valid message `message` (its raw line was just read): the matching Beast
    /// frame's sample index, else the fallback.
    fn stamp(&self, message: &[u8]) -> u64 {
        let deadline = Instant::now() + BEAST_MATCH_WAIT;
        let mut st = self.beast();
        loop {
            if let Some(pos) = st.frames.iter().position(|(m, _)| m.as_slice() == message) {
                // Both outputs follow readsb's decode order: frames before the match belong to
                // lines this wrapper did not stamp (other downlink formats, failed CRC).
                st.frames.drain(..pos);
                let (_, ticks) = st.frames.pop_front().expect("the matched frame");
                drop(st);
                if let Some(index) = self.index_of_timestamp(ticks) {
                    return index;
                }
                break;
            }
            let now = Instant::now();
            if !st.connected || st.closed || now >= deadline {
                break;
            }
            st = self
                .beast_changed
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        self.last_sample_index.load(Ordering::Relaxed)
    }
}

/// The message bytes of one `*hex;` AVR raw line.
fn avr_bytes(line: &str) -> Option<Vec<u8>> {
    let hex = line.trim().strip_prefix('*')?.strip_suffix(';')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Parses a Beast binary stream: `0x1a`, a type byte, a 6-byte big-endian timestamp, a signal
/// byte and the message, with every `0x1a` inside escaped as `0x1a 0x1a`. Calls
/// `on_frame(type, timestamp, message)` for Mode A/C (`'1'`, 2 bytes), Mode S short (`'2'`, 7)
/// and long (`'3'`, 14) frames; other types are skipped to the next frame start.
fn parse_beast(r: impl Read, mut on_frame: impl FnMut(u8, u64, &[u8])) {
    let mut bytes = BufReader::new(r).bytes().map_while(Result::ok).peekable();
    let mut at_start = false;
    loop {
        if !at_start && !bytes.by_ref().any(|b| b == 0x1a) {
            return;
        }
        at_start = false;
        let Some(kind) = bytes.next() else { return };
        let len = match kind {
            0x31 => 2,
            0x32 => 7,
            0x33 => 14,
            _ => continue,
        };
        let mut body = [0u8; 7 + 14];
        let mut filled = 0;
        while filled < 7 + len {
            let Some(b) = bytes.next() else { return };
            if b == 0x1a {
                if bytes.peek() == Some(&0x1a) {
                    bytes.next();
                } else {
                    // An unescaped frame start: the frame was truncated.
                    at_start = true;
                    break;
                }
            }
            body[filled] = b;
            filled += 1;
        }
        if at_start {
            continue;
        }
        let ticks = body[..6].iter().fold(0u64, |t, &b| (t << 8) | u64::from(b));
        on_frame(kind, ticks, &body[7..7 + len]);
    }
}

/// Accepts readsb's Beast connection and queues its Mode S frames for [`Timing::stamp`].
fn pump_beast(listener: TcpListener, timing: &Timing) {
    let Ok((stream, _)) = listener.accept() else {
        timing.set_closed();
        return;
    };
    timing.set_connected();
    parse_beast(stream, |kind, ticks, message| {
        if kind != 0x31 {
            timing.push_frame(message.to_vec(), ticks);
        }
    });
    timing.set_closed();
}

/// The stamp for one raw line (see [`Timing::stamp`]); lines this wrapper will not emit take the
/// fallback without waiting.
fn line_stamp(line: &str, timing: &Timing) -> u64 {
    match avr_bytes(line) {
        Some(b) if matches!(b.len(), 7 | 14) && crc24_remainder(&b) == 0 => timing.stamp(&b),
        _ => timing.last_sample_index.load(Ordering::Relaxed),
    }
}

/// Parses readsb's `--raw` stdout into NDJSON decode lines on our own stdout.
fn pump_stdout(stdout: impl Read, timing: &Timing, window_samples: u64) {
    let mut r = BufReader::new(stdout);
    let mut out = io::stdout().lock();
    let mut line = String::new();
    let mut cpr: HashMap<String, CprCache> = HashMap::new();
    while r.read_line(&mut line).unwrap_or(0) > 0 {
        let idx = line_stamp(&line, timing);
        if let Some(frame) = parse_avr_line(&line, &mut cpr, idx, window_samples)
            && (writeln!(out, "{}", decode_line(&frame)).is_err() || out.flush().is_err())
        {
            break; // our own stdout closed (host detaching us); nothing more to do
        }
        line.clear();
    }
}

fn spawn_readsb(extra_args: &[String], beast_port: Option<u16>) -> io::Result<Child> {
    let beast: Vec<String> = beast_port
        .map(|port| {
            vec![
                "--net".into(),
                format!("--net-connector=127.0.0.1,{port},beast_out"),
                "--net-ro-interval=0.01".into(),
                "--net-heartbeat=0".into(),
            ]
        })
        .unwrap_or_default();
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
        .args(beast)
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
    timing: &Timing,
    wait_for_beast: bool,
    ready: mpsc::Sender<()>,
) {
    // readsb's sample count so far (every byte written to its stdin, silence included).
    let mut written = 0u64;
    let silence = [0x80u8; KEEPALIVE_CHUNK_BYTES];
    let send_silence = |stdin: &mut dyn Write, written: &mut u64| {
        if stdin
            .write_all(&silence)
            .and_then(|()| stdin.flush())
            .is_err()
        {
            crash_exit("keepalive write to readsb's stdin failed; it likely died");
        }
        *written += (KEEPALIVE_CHUNK_BYTES / 2) as u64;
    };
    if wait_for_beast {
        // Hold real samples until readsb's Beast connection exists (module doc), keeping readsb
        // fed meanwhile, and write the pre-roll *after* it: readsb does not forward a message
        // decoded in its first blocks after the connector opens, so those blocks must be silence.
        // Before T-223 the pre-roll came first and, under load, was long consumed by the time
        // readsb connected 1.6 s later, which cost the chain's first squitter its Beast stamp.
        let started = Instant::now();
        let deadline = started + BEAST_CONNECT_WAIT;
        let mut connected = false;
        while Instant::now() < deadline {
            if timing.wait_connected(Duration::from_millis(200)) {
                connected = true;
                break;
            }
            send_silence(&mut readsb_stdin, &mut written);
        }
        if connected {
            eprintln!(
                "hk-plugin-readsb: Beast connected after {} ms",
                started.elapsed().as_millis()
            );
        } else {
            eprintln!(
                "hk-plugin-readsb: no Beast connection after {BEAST_CONNECT_WAIT:?}; decode stamps fall back"
            );
        }
        let preroll_to = written + PREROLL_SAMPLES;
        while written < preroll_to {
            send_silence(&mut readsb_stdin, &mut written);
        }
    }
    // Everything needed to stamp a message is in place (T-223): the main thread turns this into
    // the contract's `ready` line, and the host starts feeding.
    let _ = ready.send(());
    loop {
        match rx.recv_timeout(KEEPALIVE_INTERVAL) {
            Ok((chunk, first_index)) => {
                let n = (chunk.len() / 2) as u64;
                if readsb_stdin.write_all(&chunk).is_err() {
                    crash_exit("write to readsb's stdin failed; it likely died");
                }
                timing.placed(written, first_index, n);
                written += n;
            }
            Err(RecvTimeoutError::Timeout) => send_silence(&mut readsb_stdin, &mut written),
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

    // The Beast listener for sample-time stamps; without it decodes keep the fallback stamp.
    let beast = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("hk-plugin-readsb: no Beast listener ({e}); decode stamps fall back");
            None
        }
    };
    let beast_port = beast
        .as_ref()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port());
    let mut child = match spawn_readsb(&extra_args, beast_port) {
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
    let timing = Arc::new(Timing::new(sample_rate_hz));
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, u64)>(WRITER_BACKLOG_CHUNKS);

    let wait_for_beast = beast.is_some();
    if let Some(listener) = beast {
        // Not joined: it ends when readsb closes the connection (or never, if it never connects).
        let t = Arc::clone(&timing);
        thread::spawn(move || pump_beast(listener, &t));
    }
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let timing_for_writer = Arc::clone(&timing);
    let writer = thread::spawn(move || {
        feed_readsb(
            readsb_stdin,
            rx,
            &timing_for_writer,
            wait_for_beast,
            ready_tx,
        )
    });

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

    // The contract's readiness line (docs/stream-contract.md §9.3, T-223), written before the
    // readsb reader thread takes stdout: the writer thread has readsb's Beast connection and its
    // pre-roll in place, so every message from here on can carry its own sample time. The host
    // holds the chain's first record until this line.
    let _ = ready_rx.recv_timeout(READY_WAIT);
    println!("{}", json!({"type": "ready"}));
    let _ = io::stdout().flush();

    let timing_for_out = Arc::clone(&timing);
    let out_thread =
        thread::spawn(move || pump_stdout(readsb_stdout, &timing_for_out, cpr_window_samples));

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
                // The writer places the record by its first sample index; the fallback stamp is
                // its *last* sample (the host's offered-range check is inclusive, and one past
                // the end can read as out of range under a restricted class).
                if tx.send((converted, b.header.sample_index)).is_err() {
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

    /// Beast ticks for readsb sample count `at` (the stamp of a message starting there).
    fn ticks(at: f64) -> u64 {
        ((at / 2.4e6 + BEAST_STAMP_OFFSET_S) * BEAST_TICKS_PER_S).round() as u64
    }

    fn beast_frame(kind: u8, ticks: u64, message: &[u8]) -> Vec<u8> {
        let mut body = ticks.to_be_bytes()[2..].to_vec();
        body.push(0x40);
        body.extend_from_slice(message);
        let mut out = vec![0x1a, kind];
        for b in body {
            out.push(b);
            if b == 0x1a {
                out.push(0x1a);
            }
        }
        out
    }

    #[test]
    fn beast_frames_parse_with_escapes_and_resync_after_truncation() {
        let long: Vec<u8> = (0..14).map(|i| if i == 3 { 0x1a } else { i }).collect();
        let short = [0x58, 0x1a, 0x1a, 1, 2, 3, 4];
        let mut stream = b"garbage".to_vec();
        stream.extend(beast_frame(0x31, 7, &[0, 0]));
        stream.extend(beast_frame(0x33, 0x1a_0000_1a05, &long));
        // A frame cut off by the next frame start.
        stream.extend(&beast_frame(0x33, 9, &long)[..10]);
        stream.extend(beast_frame(0x32, 42, &short));
        stream.extend([0x1a, 0x7f, 1, 2]); // unknown type
        let mut got = Vec::new();
        parse_beast(&stream[..], |k, t, m| got.push((k, t, m.to_vec())));
        assert_eq!(
            got,
            vec![
                (0x31, 7, vec![0, 0]),
                (0x33, 0x1a_0000_1a05, long.clone()),
                (0x32, 42, short.to_vec()),
            ]
        );
    }

    #[test]
    fn beast_timestamps_map_to_stream_indices_and_silence_maps_to_nothing() {
        let t = Timing::new(2.4e6);
        let pre = PREROLL_SAMPLES as f64;
        t.placed(PREROLL_SAMPLES, 1_000_000, 65_536);
        let after_keepalive = PREROLL_SAMPLES + 65_536 + 8_192;
        t.placed(after_keepalive, 2_000_000, 1_000);
        assert_eq!(t.index_of_timestamp(ticks(pre + 100.0)), Some(1_000_100));
        assert_eq!(t.index_of_timestamp(ticks(10.0)), None, "pre-roll");
        assert_eq!(
            t.index_of_timestamp(ticks(pre + 65_536.0 + 5.0)),
            None,
            "keepalive"
        );
        assert_eq!(
            t.index_of_timestamp(ticks(after_keepalive as f64 + 999.0)),
            Some(2_000_999)
        );
        assert_eq!(t.index_of_timestamp(ticks(1e9)), None, "not written yet");
        assert_eq!(t.index_of_timestamp(0), None, "before the stamp offset");
    }

    #[test]
    fn stamps_match_frames_in_order_and_fall_back_without_a_frame() {
        let t = Timing::new(2.4e6);
        t.placed(0, 500, 10_000);
        let (a, b) = (vec![0xaa; 14], vec![0xbb; 7]);
        let started = Instant::now();
        assert_eq!(t.stamp(&a), 10_499, "no connection: fallback at once");
        assert!(started.elapsed() < BEAST_MATCH_WAIT);
        t.set_connected();
        t.push_frame(vec![0x20; 7], ticks(1.0)); // a downlink format never stamped
        t.push_frame(a.clone(), ticks(100.0));
        t.push_frame(b.clone(), ticks(200.0));
        assert_eq!(t.stamp(&a), 600);
        assert_eq!(t.stamp(&b), 700);
        // A frame that never arrives: the fallback after the match wait.
        let started = Instant::now();
        assert_eq!(t.stamp(&a), 10_499);
        assert!(started.elapsed() >= BEAST_MATCH_WAIT);
        t.set_closed();
        assert_eq!(t.stamp(&b), 10_499);
        assert_eq!(
            avr_bytes("*5d4ca853;\n"),
            Some(vec![0x5d, 0x4c, 0xa8, 0x53])
        );
        assert_eq!(avr_bytes("*5d4;"), None);
    }

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
        let timing = Timing::new(2.4e6);
        let mut out = Vec::new();
        let (ready_tx, ready_rx) = mpsc::channel::<()>();
        feed_readsb(&mut out, rx, &timing, false, ready_tx);
        assert_eq!(out.len(), 4 * WRITER_BACKLOG_CHUNKS);
        // Readiness (T-223) is reported before any real chunk is forwarded; without a Beast
        // listener there is nothing to wait for, so it is immediate.
        assert!(
            ready_rx.try_recv().is_ok(),
            "the writer never reported readiness"
        );
        // Each 2-sample chunk's last sample is the fallback stamp.
        assert_eq!(
            timing.last_sample_index.load(Ordering::Relaxed),
            100 + WRITER_BACKLOG_CHUNKS as u64 - 1 + 1
        );
        // Chunks are placed contiguously in readsb's sample count.
        assert_eq!(timing.index_at(2), Some(101));
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
