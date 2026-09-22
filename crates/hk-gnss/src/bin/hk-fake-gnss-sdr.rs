//! `hk-fake-gnss-sdr`: a **test double** for the `gnss-sdr` executable (T-323), used by
//! `tests/gnss_sdr_plugin.rs` because the dev machine has no GNSS-SDR install (no Homebrew
//! formula). It is not a receiver: it decodes nothing.
//!
//! It takes the command line `hk-plugin-gnss-sdr` gives the real binary
//! (`[--fake-mode=M] --config_file=PATH`, or `--version`), **checks the generated config** the
//! way the real receiver would need it (an `ibyte` file source whose file exists and holds whole
//! `ci8` samples, matching `internal_fs_sps`/`sampling_frequency`, an output path), and then
//! writes the files the real receiver writes — a RINEX 3.02 observation file (named by date, as
//! GNSS-SDR does, so the wrapper must find it by header) and the NMEA log — for a canned
//! six-satellite constellation, one epoch per `PVT.rinexobs_rate_ms` of input. The files are
//! formatted here by hand from the RINEX 3.02 and NMEA 0183 layouts, independently of the parser
//! in `hk_gnss::receiver`, so the test is not the parser agreeing with itself.
//!
//! Modes: `tracking` (default); `jam` (second half: every C/N0 12 dB down and five of six
//! satellites lost); `crash` (exit 3, nothing written); `hang` (sleeps until killed).
//!
//! The canned scene: GPS time 2026-09-22 10:00:00 (UTC 09:59:42), PRNs 2 5 12 15 24 29; PRN 24
//! scintillates (±4 dB alternating epochs); fixes at 51°28.6740′N 0°00.0900′W, 45 m ellipsoidal,
//! on whole seconds while at least four satellites are held.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::process::exit;

/// PRN, C/N0 dB-Hz, elevation deg, Doppler Hz.
const SATS: [(u8, f64, u32, f64); 6] = [
    (2, 44.0, 70, -1200.0),
    (5, 41.0, 55, 850.0),
    (12, 38.0, 20, 2300.0),
    (15, 46.0, 80, -300.0),
    (24, 35.0, 12, 3100.0),
    (29, 40.0, 40, -2700.0),
];
const SCINT_PRN: u8 = 24;

fn line(content: &str, label: &str) -> String {
    format!("{content:<60}{label}\n")
}

fn nmea(body: &str) -> String {
    let cs = body.bytes().fold(0u8, |a, b| a ^ b);
    format!("${body}*{cs:02X}\r\n")
}

fn main() {
    let mut mode = "tracking".to_string();
    let mut conf_path = None;
    for a in std::env::args().skip(1) {
        if a == "--version" {
            println!("gnss-sdr version 0.0.0-hk-fake");
            return;
        } else if let Some(m) = a.strip_prefix("--fake-mode=") {
            mode = m.to_string();
        } else if let Some(p) = a.strip_prefix("--config_file=") {
            conf_path = Some(p.to_string());
        } else {
            eprintln!("unknown argument {a}");
            exit(2);
        }
    }
    let Some(conf_path) = conf_path else {
        eprintln!("no --config_file");
        exit(2);
    };
    let text = std::fs::read_to_string(&conf_path).unwrap_or_else(|e| {
        eprintln!("reading config: {e}");
        exit(2)
    });
    let conf: BTreeMap<&str, &str> = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    let get = |k: &str| -> &str {
        conf.get(k).copied().unwrap_or_else(|| {
            eprintln!("config lacks {k}");
            exit(7)
        })
    };
    if get("SignalSource.implementation") != "File_Signal_Source"
        || get("SignalSource.item_type") != "ibyte"
        || get("DataTypeAdapter.implementation") != "Ibyte_To_Complex"
        || get("SignalSource.sampling_frequency") != get("GNSS-SDR.internal_fs_sps")
    {
        eprintln!("config is not an ibyte file source at the internal rate");
        exit(7);
    }
    let fs: f64 = get("SignalSource.sampling_frequency")
        .parse()
        .unwrap_or(0.0);
    let rate_ms: u64 = get("PVT.rinexobs_rate_ms").parse().unwrap_or(0);
    let bytes = std::fs::metadata(get("SignalSource.filename"))
        .map(|m| m.len())
        .unwrap_or_else(|e| {
            eprintln!("input file: {e}");
            exit(7)
        });
    if fs <= 0.0 || rate_ms == 0 || bytes % 2 != 0 {
        eprintln!("bad rate or a partial ci8 sample");
        exit(7);
    }
    let out = std::path::PathBuf::from(get("PVT.output_path"));
    let nmea_name = get("PVT.nmea_dump_filename").to_string();
    eprintln!("fake gnss-sdr: {bytes} bytes at {fs} sps, mode {mode}");

    match mode.as_str() {
        "crash" => exit(3),
        "hang" => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        "tracking" | "jam" => {}
        other => {
            eprintln!("unknown mode {other}");
            exit(2);
        }
    }

    let duration_ms = ((bytes / 2) as f64 / fs * 1000.0).floor() as u64;
    let n_epochs = duration_ms / rate_ms;
    let mut rinex = line(
        "     3.02           OBSERVATION DATA    G (GPS)",
        "RINEX VERSION / TYPE",
    );
    rinex += &line("hk-fake-gnss-sdr", "PGM / RUN BY / DATE");
    rinex += &line("G    4 C1C L1C D1C S1C", "SYS / # / OBS TYPES");
    rinex += &line("    18", "LEAP SECONDS");
    rinex += &line("", "END OF HEADER");
    let mut log = String::new();
    // GSV: two sentences of up to four satellites.
    for (i, chunk) in SATS.chunks(4).enumerate() {
        let mut b = format!("GPGSV,2,{},{:02}", i + 1, SATS.len());
        for &(prn, cn0, el, _) in chunk {
            let _ = write!(b, ",{prn:02},{el:02},180,{:02}", cn0 as u32);
        }
        log += &nmea(&b);
    }
    for k in 0..n_epochs {
        let ms = k * rate_ms;
        let jammed = mode == "jam" && k >= n_epochs / 2;
        let sats: Vec<_> = SATS
            .iter()
            .filter(|s| !jammed || s.0 == 29)
            .map(|&(prn, cn0, _, dop)| {
                let mut c = cn0;
                if jammed {
                    c -= 12.0;
                }
                if prn == SCINT_PRN {
                    c += if k % 2 == 0 { 4.0 } else { -4.0 };
                }
                let pr = 20_000_000.0 + f64::from(prn) * 100_000.0 + ms as f64 * 0.5;
                (prn, pr, pr / 0.190_293_672_798_365, dop, c)
            })
            .collect();
        let sec = 0.0 + (ms % 60_000) as f64 / 1000.0;
        let min = ms / 60_000;
        let _ = writeln!(
            rinex,
            "> 2026 09 22 10 {min:02}{sec:11.7}  0{:3}",
            sats.len()
        );
        for (prn, pr, cyc, dop, c) in &sats {
            let _ = writeln!(
                rinex,
                "G{prn:02}{pr:14.3}  {cyc:14.3}  {dop:14.3}  {c:14.3}  "
            );
        }
        if ms % 1000 == 0 && sats.len() >= 4 {
            // UTC = GPS − 18 s: 10:00:00 GPS is 09:59:42 UTC.
            let utc_s = 9 * 3600 + 59 * 60 + 42 + ms / 1000;
            let (h, m, s) = (utc_s / 3600, (utc_s / 60) % 60, utc_s % 60);
            log += &nmea(&format!(
                "GPGGA,{h:02}{m:02}{s:02}.00,5128.6740,N,00000.0900,W,1,{:02},1.0,0.0,M,45.0,M,,",
                sats.len()
            ));
        }
    }
    let write = |name: &str, body: &str| {
        std::fs::write(out.join(name), body).unwrap_or_else(|e| {
            eprintln!("writing {name}: {e}");
            exit(8)
        })
    };
    write("GSDR265a00.26O", &rinex);
    write(&nmea_name, &log);
}
