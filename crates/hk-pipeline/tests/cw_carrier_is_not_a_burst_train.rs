//! T-980, through the mock SDR: **a carrier with nothing on it is a line, and a line is not a
//! burst train.**
//!
//! Found 2026-09-25 by the explorer on live HackRF at 315 MHz (TPMS). The oracle held a steady CW
//! carrier at 314.988 MHz, 53 dB over the median FFT bin, with **no bursts in it**. The app showed
//! one Confirmed row — 314.9997 MHz, 55.2 kHz wide, `unknown` — and the fsk-burst chains minted
//! **169–186 "fsk bursts"** at 314.99–315.07 MHz, 0 of them framed, with symbol-rate guesses
//! spread from 1200 to 9600 Bd. Two separate defects, one scene:
//!
//! 1. **Nothing said "carrier".** A continuous emission no wider than the analysis window's own
//!    main lobe carries no modulation the resolution can see, and the width it reports is the
//!    window's rather than its own — 55.2 kHz is a one-to-two-bin line on a coarse survey, not a
//!    55 kHz occupied bandwidth, and 12 kHz is a fraction of such a bin. The row said `unknown`
//!    and offered no explanation at all.
//! 2. **Every member box became a burst.** The chain demodulates one box per member of a track and
//!    counts what comes back; nothing asked whether the box was ever an emission that *started and
//!    stopped*. A track that never bursts still yields one "burst" per member, each with a symbol
//!    rate fitted to noise.
//!
//! Both halves are asserted here on the `tone` scene (a CW carrier in white noise) served through
//! the mock SDR, blind: the truth annotations are stripped before the pipeline sees the recording
//! ([`blind_replay_config`]) and are used only to say where the carrier really was.
//!
//! On the code before T-980 the first test found the row but no explanation, and the second minted
//! **973** bursts.

mod common;

use common::*;
use hk_core::Pacing;
use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::InventoryQuery;
use hk_pipeline::family::{self, CW_LINE_MAX_BINS, UNMODULATED_CARRIER};
use serde_json::json;

/// Where the carrier really is: the tuned centre plus the scene's offset. The pipeline is never
/// told either — it is served the blinded recording and has to find the line itself.
const CENTER_HZ: f64 = 315e6;
const OFFSET_HZ: f64 = -12e3;
const CARRIER_HZ: f64 = CENTER_HZ + OFFSET_HZ;

/// The `tone` scene: one unmodulated carrier for the whole recording, in white noise.
///
/// `noise_dbfs` is the knob that decides whether this is a *clean* carrier or a **strong** one on
/// an 8-bit front end. At −63 dBFS of noise a −10 dBFS carrier has nothing left to dither the
/// quantiser with, so its own quantisation products spray the band as dozens of steady lines —
/// which is exactly the HackRF the explorer was holding, and what turned one carrier into 88
/// tracks and 973 "bursts".
fn tone(noise_dbfs: f64, duration_s: f64) -> SynthRequest {
    SynthRequest::new("tone")
        .seed(7)
        .param("sample_rate", 2e6)
        .param("center_hz", CENTER_HZ)
        .param("offset_hz", OFFSET_HZ)
        .param("power_dbfs", -10.0)
        .param("noise_dbfs", noise_dbfs)
        .param("duration_s", duration_s)
}

/// A clean CW carrier is **one** inventory row: a line at the carrier, no wider than the analysis
/// window can resolve, explained "unmodulated carrier (possible spur/LO)" — never an `unknown` of
/// tens of kHz sitting a bin off the peak.
#[test]
fn a_cw_carrier_is_one_line_explained_as_an_unmodulated_carrier() {
    let out = synth_or_skip!(tone(-40.0, 3.0));
    let meta = out.fixture(0).unwrap().meta_path;
    let dir = TempDir::new("t980-cw-line");
    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);

    let bin_hz = s.resolution.bin_hz;
    assert!(
        bin_hz > 0.0,
        "no analysis resolution to read the width against"
    );
    let repo = repo(&dir.0);
    let rows = inventory(&repo, InventoryQuery::default());
    for e in &rows {
        eprintln!(
            "[T-980] row {:.1} Hz, {:.1} Hz ({:.1} bins), {:?}, family {:?}",
            e.emitter.f_center_hz,
            e.emitter.bandwidth_hz,
            e.emitter.bandwidth_hz / bin_hz,
            e.lifecycle,
            e.family,
        );
    }
    // One carrier on the air, one row for it. Near-duplicates merged, no ghosts (ADR-0017).
    assert_eq!(rows.len(), 1, "one carrier, {} rows", rows.len());
    let row = &rows[0];

    // The estimate is a CW line: centred on the peak to within a bin, and no wider than a
    // windowed tone can measure.
    let off_hz = (row.emitter.f_center_hz - CARRIER_HZ).abs();
    assert!(
        off_hz <= bin_hz,
        "centre {:.1} Hz is {off_hz:.1} Hz ({:.2} bins) off the carrier",
        row.emitter.f_center_hz,
        off_hz / bin_hz,
    );
    let bins = row.emitter.bandwidth_hz / bin_hz;
    assert!(
        bins <= CW_LINE_MAX_BINS,
        "{:.1} Hz is {bins:.1} analysis bins: too wide to be an unmodulated carrier",
        row.emitter.bandwidth_hz,
    );

    // ... and it is explained as one, ranked, with the words a reader needs.
    let ranked = family::explanations(&repo, row.emitter.id).unwrap();
    let top = ranked
        .first()
        .unwrap_or_else(|| panic!("no explanation at all for a {bins:.1}-bin continuous line"));
    eprintln!("[T-980] explanations {ranked:?}");
    assert_eq!(top.service, UNMODULATED_CARRIER, "top explanation {top:?}");
    assert_eq!(top.label, "unmodulated carrier (possible spur/LO)");
    // Shape evidence suggests and never decides: the status stays `unknown` and the reason says
    // why the width is readable at all.
    assert!(top.has_flag("shape-only"), "{:?}", top.flags);
    assert_eq!(top.status_evidence_confidence, 0.0);

    // Nothing bursty happened, so nothing bursty was demodulated.
    assert_eq!(s.counter("/chains/fsk_bursts"), 0);
}

/// A strong carrier on an 8-bit front end — the field's own conditions, quantisation lines and all
/// — mints **no** FSK bursts, however many tracks its spray opens. Before T-980 this scene
/// produced 973.
#[test]
fn a_steady_carrier_mints_no_fsk_bursts() {
    let out = synth_or_skip!(tone(-63.0, 6.0));
    let meta = out.fixture(0).unwrap().meta_path;
    let dir = TempDir::new("t980-cw-bursts");
    let (cfg, replay, _input) = blind_replay_config(&dir.0, &meta, json!({}), Pacing::Unpaced);
    let s = start(cfg, replay).wait().unwrap();
    eprintln!("{}", s.to_text());
    assert!(s.errors.is_empty(), "{:?}", s.errors);

    // The chains really did run on this scene — otherwise "0 bursts" would prove nothing.
    let attached = s.counter("/chains/attached");
    assert!(attached > 0, "no chain attached, so nothing was refused");
    let refused = s.counter("/chains/fsk_not_a_burst");
    assert_eq!(
        s.counter("/chains/fsk_bursts"),
        0,
        "a scene whose only emission is a steady carrier produced bursts ({refused} refused)",
    );
    // ... and 0 because every box was asked and answered, not because none was offered.
    assert!(
        refused > 0,
        "{attached} chains attached but no box was even offered",
    );
    // A refusal is an answer, not a failure.
    assert_eq!(s.counter("/chains/errors"), 0);
    assert_eq!(s.counter("/chains/crc_valid"), 0);
}
