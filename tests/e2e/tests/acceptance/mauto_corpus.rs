//! **The corpus sufficiency argument, made checkable** (T-627; docs/22 §4.5, §6.2, §6.4) —
//! `RESEARCH-002`, `SIGNAL-052`.
//!
//! ```text
//! cargo nextest run -p hk-e2e -E 'binary(acceptance_mauto) & test(/mauto_corpus/)'
//! ```
//!
//! Three mechanisms, none of which is a scene:
//!
//! 1. **The coverage manifest** — [`manifest`]. Every run renders the declared axis product of
//!    docs/22 §2 with one mark per cell (`populated n=…`, `unreachable <ticket>: <reason>`, or
//!    `UNMARKED`), writes it to `target/mauto-coverage-manifest.txt`, and compares it with the
//!    committed `tests/e2e/mauto-coverage-manifest.txt`. A change in coverage is therefore a diff
//!    somebody has to bless (`HK_BLESS_MANIFEST=1`), never a silent drift. An `UNMARKED` cell
//!    fails the suite: it is a cell nobody thought about, a failure of the manifest rather than of
//!    the engine. **The M-12 review reads the manifest before any pass rate.**
//! 2. **The sealed hold-out** — [`hk_e2e::corpus::sealed`]. `sha256(scene_id ‖ seed) mod 5 == 0`
//!    seals a seed until M-12. Every generated row here draws its seeds through
//!    [`SeedPlan`], so the open suite never runs a sealed seed.
//! 3. **The expected-failure rows** — docs/22 §6.2 item 3. One row per family that has an engine
//!    today, placed deliberately **past the edge (6 dB at 112 symbols)** and asserted to fail
//!    **honestly**: deepest verdict below `framed`, `reason: nothing-scored`, no over-claim. A
//!    corpus with no such rows cannot tell a strong engine from a permissive threshold.
//!
//! # What the expected-failure rows run, and what they cannot yet run
//!
//! They drive the **real blind blocks the pipeline calls** — `hk_demod::fsk::structure::measure`
//! (S1/S2: clock line, level count, abstention) and, for a four-level alphabet, the C4FM
//! demodulator plus every framing in `hk_detect::trunk::CC_FRAMINGS` (S4/S5) — and record what
//! they reached as a `docs/07` [`Resolution`]. They are component-tier, not end-to-end through
//! the mock SDR, and they say so: **no MAUTO engine yet writes a sealed negative result for an
//! emitter whose search finished without a framing** (the only `EmitterSynthesis` writer is the
//! trunk control-channel analysis, which runs only after a framing confirmed). That is T-567's
//! sealed Resolution, on top of T-565's search; when it lands, these rows move behind the device
//! interface and assert on `/api/analyze`'s `resolution` instead of on the blocks.
//!
//! The IQ is generated here, deterministically from the seed, because a 1120-sample CPFSK burst
//! does not warrant a Python round trip; the truth (family, level count, symbol rate) is held by
//! the row and read only by [`honest_failure`], after the blocks ran.

use std::process::Command;

use hk_demod::fsk::c4fm::{C4fmConfig, C4fmDemod};
use hk_demod::fsk::structure::{self, FmStructure, StructureError};
use hk_detect::trunk::{CC_FRAMINGS, CcConfirmer};
use hk_e2e::corpus::{
    self, Cell, EXPECTED_FAILURE_PLANE, Manifest, Row, SeedPlan, Ticket, Unreachable,
};
use hk_model::repo::synthesis::{Resolution, ResolutionKind, ResolutionReason, Verdict};
use num_complex::Complex32;

const USE_CASES: &str = "RESEARCH-002 / SIGNAL-052 (T-627)";

// ------------------------------------------------------------------------------------------
// The declared corpus: rows that run, and regions declared unreachable.
// ------------------------------------------------------------------------------------------

/// Seeds drawn for every expected-failure family, before the hold-out splits them.
const EF_SEEDS: std::ops::RangeInclusive<u64> = 1..=10;

/// The scene-family id the hold-out hashes for an expected-failure family.
fn ef_scene_id(family: &str) -> String {
    format!("mauto/expected-failure/{family}@6dB/112sym")
}

/// The expected-failure families that have an engine today, with the test that runs each.
const EF_FAMILIES: &[(&str, &str)] = &[
    (
        "2fsk",
        "mauto_corpus::expected_failure_2fsk_at_6db_and_112_symbols_fails_honestly",
    ),
    (
        "4fsk-c4fm",
        "mauto_corpus::expected_failure_c4fm_at_6db_and_112_symbols_fails_honestly",
    ),
    (
        "msk",
        "mauto_corpus::expected_failure_msk_at_6db_and_112_symbols_fails_honestly",
    ),
];

/// The `signal_087` scene (T-545/T-546): C4FM, 20 dB, 3 s at 4800 Bd (≥ 4096 symbols),
/// template-fixed P25 framing with its CRC-16, CFO 0 on the clean-clock run, templates on.
const P087_SCENE: &str = "trunk_encrypted_control_channel";
const P087_SEEDS: &[u64] = &[545];
const P087_TESTS: &[&str] = &[
    "signal_087::a_the_emission_is_detected_blind_as_a_time_frequency_region",
    "signal_087::b_the_modulation_symbol_rate_and_deviation_are_estimated_from_the_signal",
    "signal_087::c_the_demod_and_decode_pipeline_is_auto_selected_from_the_measurements",
    "signal_087::d_the_decode_reaches_the_emission_the_run_detected",
    "signal_087::e_a_sensible_explanation_ranks_among_the_top_suggestions",
    "signal_087::e2_the_explanation_rests_on_measured_evidence_not_only_the_allocation",
    "signal_087::f_the_receiver_clock_error_is_measured_not_assumed_zero",
];

fn rows() -> Vec<Row> {
    let mut rows = vec![Row {
        id: "P-087",
        tests: P087_TESTS,
        // A fixed, hand-built acceptance scene whose seed pre-dates the seal — and 545 does hash
        // sealed (`sha256(... ‖ 545) mod 5 == 0`). It is run in the open by `signal_087`, so the
        // manifest counts it as run and says why in words; reporting n=0 for a seed that runs
        // every time would be the manifest lying in the other direction.
        seeds: SeedPlan::new(P087_SCENE, P087_SEEDS.iter().copied()),
        fixed_seeds: Some("P-087 seed 545 pre-dates the seal (T-545) and is run in the open"),
        cells: vec![
            Cell::new(
                "A1xA2xA4",
                &[("A1", "4fsk-c4fm"), ("A2", "20dB"), ("A4", "4096")],
            ),
            Cell::new("A1xA5", &[("A1", "4fsk-c4fm"), ("A5", "template-fixed")]),
            Cell::new("A1xA6", &[("A1", "4fsk-c4fm"), ("A6", "0")]),
            Cell::new("A1xA7", &[("A1", "4fsk-c4fm"), ("A7", "crc16-fixed")]),
            Cell::new("A1xA8", &[("A1", "4fsk-c4fm"), ("A8", "on")]),
        ],
    }];
    for (family, test) in EF_FAMILIES {
        rows.push(Row {
            id: match *family {
                "2fsk" => "EF-2fsk",
                "4fsk-c4fm" => "EF-4fsk-c4fm",
                _ => "EF-msk",
            },
            tests: std::slice::from_ref(test),
            seeds: SeedPlan::new(&ef_scene_id(family), EF_SEEDS),
            fixed_seeds: None,
            cells: vec![
                Cell::new(EXPECTED_FAILURE_PLANE, &[("A1", family)]),
                Cell::new("A1xA2xA4", &[("A1", family), ("A2", "6dB"), ("A4", "112")]),
                // Truth is random symbols: no framing, no check — structure the engine must NOT
                // find. Templates play no part in the blocks these rows drive.
                Cell::new("A1xA5", &[("A1", family), ("A5", "out-of-catalogue")]),
                Cell::new("A1xA6", &[("A1", family), ("A6", "0")]),
                Cell::new("A1xA7", &[("A1", family), ("A7", "none")]),
            ],
        });
    }
    // T-568: the negative-control populations, run blind through the mock SDR.
    rows.extend(crate::mauto_negatives::manifest_rows());
    // T-863 (MAUTO M-12): ADR-0015 §7's generic FSK/OOK sweep and partial-quality rows, run blind
    // through the mock SDR and `/api/analyze`. Populated = the jobs ran; while
    // `server_backend()` is None every one answers not-searched, and the suite's report says so.
    rows.extend(crate::mauto_eval::manifest_rows());
    rows
}

/// No production search backend: the sweep and recall grid for a family with blocks but no
/// search. T-565 (the trace inside the beam) landed; what is missing is an owner-less piece —
/// `hk_pipeline::synth::jobs::server_backend()` is `None`, nothing implements the engine's
/// `Evaluator` over acquired IQ, so every `/api/analyze` job ends `failed / no_evaluator`.
const NO_ENGINE: &str = "no production search backend: server_backend() is None (no Evaluator over \
                         acquired IQ), so every /api/analyze job ends no_evaluator and the S1 \
                         sweep / recall grid cannot run";

/// OOK/ASK now has a generator (T-863) but the same missing backend.
const NO_BACKEND_OOK: &str = "OOK generator exists (hkpy.synth generic_fsk_sweep, T-863) and the S7 \
                              rows run it at 6/10/20 dB bursts, templates on and off; every other \
                              cell needs the production search backend (server_backend() is None)";

/// The declared-unreachable regions. **Order matters: first match wins**, so narrow before wide.
/// Every declaration names its family; there is no catch-all.
fn declarations() -> Vec<Unreachable> {
    let mut d = Vec::new();
    // --- Families with no generator at all.
    d.push(Unreachable {
        family: "ook-ask",
        planes: &[],
        when: &[],
        reason: NO_BACKEND_OOK,
        ticket: Ticket::Unfiled,
    });
    // --- Negative families whose generator now exists (T-623 N3, T-624 N2): the gap is no
    // longer the scene but the suite that runs it through an engine and asserts the verdict.
    for (fam, reason) in [
        (
            "ofdm",
            "N3 negative: run blind by the NEG row (hkpy.synth ofdm_nonstandard_cp, T-568: 0 solved, \
             framed only on a measured framing); the recall grid does not apply to it, and its \
             `unsupported-structure` + missing_block answer needs the sealed Resolution",
        ),
        (
            "dsss",
            "N3 negative: run blind by the NEG row (hkpy.synth dsss_m_sequence, T-568: 0 solved, \
             framed only on a measured framing); the recall grid does not apply to it, and its \
             `unsupported-structure` + missing_block answer needs the sealed Resolution",
        ),
        (
            "16qam",
            "N3 negative: run blind by the NEG row (hkpy.synth qam16_unframed, T-568: 0 solved, \
             framed only on a measured framing); the recall grid does not apply to it, and its \
             `unsupported-structure` + missing_block answer needs the sealed Resolution",
        ),
    ] {
        d.push(Unreachable {
            family: fam,
            planes: &[],
            when: &[],
            reason,
            ticket: Ticket::Filed("T-567"),
        });
    }
    for (fam, reason) in [
        (
            "fm-voice",
            "N2 negative: run blind by the NEG row (hkpy.synth nbfm_voice, T-568: 0 labels >= \
             framed); the recall grid does not apply to it, and its `nothing-scored`, \
             deepest_verdict <= demodulated answer needs the sealed Resolution",
        ),
        (
            "am-voice",
            "N2 negative: run blind by the NEG row (hkpy.synth am_voice, T-568: 0 labels >= \
             framed); the recall grid does not apply to it, and its `nothing-scored`, \
             deepest_verdict <= demodulated answer needs the sealed Resolution",
        ),
    ] {
        d.push(Unreachable {
            family: fam,
            planes: &[],
            when: &[],
            reason,
            ticket: Ticket::Filed("T-567"),
        });
    }
    d.push(Unreachable {
        family: "thermal-noise",
        planes: &[],
        when: &[],
        reason: "the structureless population (N1) is the NEG plane's n1-thermal row (T-568); \
                 SNR/support/check/CFO and an edge to fail past do not apply to it, and its full-n \
                 run (docs/22 n = 400) is the false-confirm suite's",
        ticket: Ticket::Filed("T-576"),
    });
    d.push(Unreachable {
        family: "css-lora",
        planes: &[],
        when: &[],
        reason: "LoRa generator exists (lora_ism_burst) but no MAUTO block dechirps or scores CSS \
                 and there is no production search backend, so no row is wired; P9 would also stay \
                 report-only, because T-619 measured the calibrated-bit discount on AM/OOK and \
                 C4FM only, not CSS (docs/22 §7.2, docs/21 §10)",
        ticket: Ticket::Unfiled,
    });
    d.push(Unreachable {
        family: "cw",
        planes: &[],
        when: &[],
        reason: "tone generator exists but no MAUTO block scores a keyed carrier and no row is \
                 wired",
        ticket: Ticket::Unfiled,
    });
    // --- Families with blocks. The check axis: T-622 put CRC width / off-catalogue polynomial /
    // constant payload on the 2-level FSK generator (`fsk_burst_train`, which also yields MSK at
    // deviation = rate / 4), so for 2-FSK the gap is now the confirm-gate rows that consume it
    // (ADR-0022 §10.1 A3 (b)/(c), T-576's recall control). The C4FM generators (the trunk
    // scenes) still carry only the fixed P25 framing: that is a generator gap nobody owns.
    d.push(Unreachable {
        family: "2fsk",
        planes: &["A1xA7"],
        when: &[],
        reason: "check parameterisation exists on fsk_burst_train (T-622) but no row runs it: the \
                 CRC-8 / off-catalogue-poly / constant-payload rows are the confirm-gate recall \
                 control of the false-confirm suite, which needs the derived ConfirmPolicy",
        ticket: Ticket::Filed("T-576"),
    });
    d.push(Unreachable {
        family: "msk",
        planes: &["A1xA7"],
        when: &[],
        reason: "check parameterisation exists on fsk_burst_train at h = 0.5 (T-622) but no row \
                 runs it: no production search backend binds or refuses a check on an MSK emitter",
        ticket: Ticket::Unfiled,
    });
    d.push(Unreachable {
        family: "4fsk-c4fm",
        planes: &["A1xA7"],
        when: &[],
        reason: "check parameterisation exists on c4fm_burst_train (T-850) but no row runs it: no \
                 production search backend binds or refuses a check on a C4FM emitter",
        ticket: Ticket::Unfiled,
    });
    for fam in ["2fsk", "4fsk-c4fm", "msk"] {
        // The F ladder: the generator is built (hkpy.synth.fill, T-625) and T-619 measured the
        // C4FM discount, but F's report (C-R pass rate, resolution.reason per fill level) and its
        // bar (the under-filled cell flagged) are read off a synthesis result.
        d.push(Unreachable {
            family: fam,
            planes: &["A1xA3"],
            when: &[],
            reason: "F ladder generator exists (hkpy.synth.fill, T-625) but no row runs it: its \
                     per-fill C-R pass rate and resolution.reason need the production search backend",
            ticket: Ticket::Unfiled,
        });
        d.push(Unreachable {
            family: fam,
            planes: &[],
            when: &[],
            reason: NO_ENGINE,
            ticket: Ticket::Unfiled,
        });
    }
    // --- The negative plane (T-568): the two sub-populations with no IQ.
    d.push(Unreachable {
        family: "n3-css",
        planes: &[],
        when: &[],
        reason: "CSS is P9 (docs/22 §4.3): lora_ism_burst exists but no MAUTO block dechirps or \
                 scores CSS, so an unsupported-structure answer on it cannot yet be told from a \
                 missing block",
        ticket: Ticket::Unfiled,
    });
    d.push(Unreachable {
        family: "n4-terminator-50ohm",
        planes: &[],
        when: &[],
        reason: "SKIPPED: the 50-ohm terminator capture is a user action, outstanding (docs/22 \
                 §10); until it lands the receiver-only null is UNMEASURED and N4 is the 433 MHz \
                 antenna window alone",
        ticket: Ticket::Filed("T-375"),
    });
    d
}

/// Builds the manifest from the declared corpus.
fn manifest() -> Manifest {
    let axes = corpus::docs22_axes();
    let declared = corpus::declared_cells(&axes, corpus::DOCS22_PLANES);
    Manifest::build(declared, &rows(), &declarations())
}

fn golden_path() -> std::path::PathBuf {
    hk_e2e::paths::repo_root().join("tests/e2e/mauto-coverage-manifest.txt")
}

// ------------------------------------------------------------------------------------------
// 1. The manifest.
// ------------------------------------------------------------------------------------------

#[test]
fn manifest_every_declared_cell_is_marked_and_the_manifest_is_the_committed_one() {
    let m = manifest();
    let text = m.render();
    // Emitted every run, where the next run can diff against it.
    let out = hk_e2e::paths::target_dir().join("mauto-coverage-manifest.txt");
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&out, &text);
    let (p, u, unfiled, un) = m.counts();
    eprintln!(
        "[{USE_CASES}] coverage manifest: {} cells, populated {p}, declared-unreachable {u} \
         (UNFILED {unfiled}), unmarked {un} -> {}",
        m.cells.len(),
        out.display(),
    );

    let unmarked: Vec<String> = m
        .unmarked()
        .iter()
        .map(|c| format!("{} [{}]", c.plane, c.key()))
        .collect();
    assert!(
        unmarked.is_empty(),
        "{USE_CASES}: {} cell(s) are UNMARKED -- nobody thought about them. This is a failure of \
         the manifest, not of the engine: populate each or declare it unreachable with a reason \
         and a ticket.\n{}",
        unmarked.len(),
        unmarked.join("\n"),
    );

    let golden = golden_path();
    if std::env::var("HK_BLESS_MANIFEST").is_ok_and(|v| v == "1") {
        std::fs::write(&golden, &text).expect("bless the committed manifest");
        eprintln!("blessed {}", golden.display());
        return;
    }
    let committed = std::fs::read_to_string(&golden).unwrap_or_default();
    if committed != text {
        let diff: Vec<String> = committed
            .lines()
            .zip(text.lines())
            .filter(|(a, b)| a != b)
            .take(20)
            .map(|(a, b)| format!("- {a}\n+ {b}"))
            .collect();
        panic!(
            "{USE_CASES}: the coverage manifest changed. Coverage moving is a reviewed event, not \
             a drift: diff {} against {} and, if the change is intended, re-run with \
             HK_BLESS_MANIFEST=1 and commit it.\nfirst differing lines (committed {} lines, now \
             {}):\n{}",
            out.display(),
            golden.display(),
            committed.lines().count(),
            text.lines().count(),
            diff.join("\n"),
        );
    }
}

#[test]
fn manifest_is_byte_identical_run_to_run() {
    assert_eq!(manifest().render(), manifest().render());
}

/// A `populated` mark is a claim that tests ran. Check the claim against this very binary: every
/// test a row names must exist and must not be `#[ignore]`d, or the cell is populated on paper.
#[test]
fn manifest_populated_rows_name_tests_that_exist_and_are_not_ignored() {
    let exe = std::env::current_exe().expect("current test binary");
    let list = |extra: &[&str]| -> Vec<String> {
        let out = Command::new(&exe)
            .args(["--list", "--format", "terse"])
            .args(extra)
            .output()
            .expect("list this binary's tests");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.strip_suffix(": test").map(str::to_owned))
            .collect()
    };
    let all = list(&[]);
    let ignored = list(&["--ignored"]);
    assert!(!all.is_empty(), "the test binary listed no tests");
    for row in rows() {
        for t in row.tests {
            assert!(
                all.iter().any(|x| x == t),
                "{USE_CASES}: row {} populates cells through `{t}`, which is not a test in this \
                 binary",
                row.id,
            );
            assert!(
                !ignored.iter().any(|x| x == t),
                "{USE_CASES}: row {} populates cells through `{t}`, which is #[ignore]d -- \
                 populated on paper only",
                row.id,
            );
        }
    }
}

/// A declared-unreachable cell names an owner, and that owner must be a real **open** ticket: a
/// declaration citing a done ticket is stale (the gap should now be fillable), and one citing an
/// id that does not exist hides an unowned gap behind a plausible number. Absence of an owner is
/// spelled `UNFILED`, never a wrong id.
#[test]
fn manifest_declarations_cite_open_tickets_and_none_is_a_catch_all() {
    let yaml = std::fs::read_to_string(hk_e2e::paths::repo_root().join("docs/tasks.yaml"))
        .expect("read docs/tasks.yaml");
    let axes = corpus::docs22_axes();
    let families = &axes[0];
    let negatives = axes.iter().find(|a| a.id == "NP").expect("the NP axis");
    for d in declarations() {
        assert!(
            families.levels.contains(&d.family) || negatives.levels.contains(&d.family),
            "declaration names unknown family {}",
            d.family
        );
        if let Ticket::Filed(t) = d.ticket {
            let status = corpus::ticket_status(&yaml, t);
            assert!(
                matches!(status.as_deref(), Some(s) if s != "done" && s != "cancelled"),
                "{USE_CASES}: the declaration for {} ({}) cites {t}, whose board status is {:?}; \
                 a gap must cite an OPEN owner (or Ticket::Unfiled)",
                d.family,
                d.reason,
                status,
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// 2. The hold-out.
// ------------------------------------------------------------------------------------------

/// The rule is a hash, so its selection is pinned by vectors: anyone re-implementing it (a Python
/// generator deciding what to seal, a reviewer at M-12) must reproduce these residues exactly.
#[test]
fn holdout_rule_is_the_hash_and_its_selection_is_reproducible() {
    // sha256("" || be_u64(0)) = af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc
    // mod 5 is computed independently below from the hex, not from the function under test.
    let hex = "af5570f5a1810b7af78caf4bc70a660f0df51e42baf91d4de5b2328de0e83dfc";
    let by_hand = (0..hex.len())
        .step_by(2)
        .map(|i| u32::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .fold(0u32, |r, b| (r * 256 + b) % 5);
    assert_eq!(corpus::holdout_residue("", 0), by_hand);

    // Reproducible: the same inputs give the same split, whatever order they are asked in.
    let a = SeedPlan::new("mauto/expected-failure/2fsk@6dB/112sym", 1..=10);
    let b = SeedPlan::new("mauto/expected-failure/2fsk@6dB/112sym", (1..=10).rev());
    let mut bs = b.sealed.clone();
    bs.sort_unstable();
    assert_eq!(a.sealed, bs);
    // The scene id is part of the key: two families do not share a sealed set by construction.
    let fams: Vec<Vec<u64>> = EF_FAMILIES
        .iter()
        .map(|(f, _)| SeedPlan::new(&ef_scene_id(f), EF_SEEDS).sealed)
        .collect();
    eprintln!("[{USE_CASES}] sealed expected-failure seeds per family: {fams:?}");
}

#[test]
fn holdout_seals_a_fifth_and_the_open_suite_never_runs_a_sealed_seed() {
    let plan = SeedPlan::new("mauto/holdout-rate-check", 0..10_000);
    let frac = plan.sealed.len() as f64 / 10_000.0;
    // Binomial sd at p = 0.2, n = 10 000 is 0.004; 5 sd either side.
    assert!(
        (frac - 0.2).abs() < 0.02,
        "{USE_CASES}: the hold-out sealed {frac:.4} of 10 000 seeds, not ~0.20"
    );
    assert_eq!(plan.open.len() + plan.sealed.len(), 10_000);
    assert!(plan.open.iter().all(|s| !plan.sealed.contains(s)));
    if !corpus::unsealed() {
        for row in rows().into_iter().filter(|r| r.fixed_seeds.is_none()) {
            let run = row.seeds.runnable();
            assert!(
                run.iter().all(|s| !row.seeds.sealed.contains(s)),
                "{USE_CASES}: row {} would run a sealed seed before M-12",
                row.id
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// 3. The expected-failure rows.
// ------------------------------------------------------------------------------------------

/// Deterministic xorshift64* — the row's only randomness, seeded by the row's seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn uniform(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }
    fn gauss(&mut self) -> f64 {
        let (u, v) = (self.uniform(), self.uniform());
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// One expected-failure family's generator parameters **and hidden truth**.
struct EfScene {
    family: &'static str,
    /// Normalised levels (outer = ±1).
    levels: &'static [f64],
    symbol_rate_bd: f64,
    outer_deviation_hz: f64,
}

impl EfScene {
    /// Carson bandwidth: what the SNR is defined over.
    fn bandwidth_hz(&self) -> f64 {
        2.0 * (self.outer_deviation_hz + 0.5 * self.symbol_rate_bd)
    }
}

const FS_HZ: f64 = 48_000.0;
/// The S0 channel width the blocks are handed: the 12.5 kHz raster channel the trunk chain hands
/// `measure_fm_structure`, wider than every scene's Carson bandwidth. **Not** the Carson width
/// itself — handed exactly that, `structure::measure` reads a clean 20 dB h = 1 2-FSK emission as
/// four levels (an engine over-claim this ticket found and reports; see the T-627 result), and
/// the control below would then be testing that defect instead of the generator.
const CHANNEL_BW_HZ: f64 = 12_500.0;
const SNR_DB: f64 = 6.0;
const SYMBOLS: usize = 112;
/// Noise-only samples ahead of the emission, for the S0 energy measurement.
const LEAD: usize = 2048;
/// S0: energy exists when the emission window's power exceeds the noise-only window's by this
/// ratio. **A priori**: a 1120-sample complex power estimate has a relative sd of 3 %, so 1.25 is
/// > 8 sd above noise, while 6 dB in a Carson bandwidth of 1/5–1/7 of 48 kHz gives 1.6–1.8.
const ENERGY_RATIO: f64 = 1.25;

const EF_2FSK: EfScene = EfScene {
    family: "2fsk",
    levels: &[-1.0, 1.0],
    symbol_rate_bd: 4800.0,
    outer_deviation_hz: 2400.0,
};
const EF_C4FM: EfScene = EfScene {
    family: "4fsk-c4fm",
    levels: &[-1.0, -1.0 / 3.0, 1.0 / 3.0, 1.0],
    symbol_rate_bd: 4800.0,
    outer_deviation_hz: 1800.0,
};
const EF_MSK: EfScene = EfScene {
    family: "msk",
    levels: &[-1.0, 1.0],
    symbol_rate_bd: 4800.0,
    // h = 0.5: deviation = rate / 4.
    outer_deviation_hz: 1200.0,
};

/// `LEAD` noise-only samples, then `symbols` symbols of continuous-phase FSK at `snr_db` in the
/// scene's Carson bandwidth, unit signal power.
fn generate(scene: &EfScene, seed: u64, snr_db: f64, symbols: usize) -> (Vec<Complex32>, usize) {
    let mut rng = Rng::new(seed);
    let sps = (FS_HZ / scene.symbol_rate_bd).round() as usize;
    let n_sig = symbols * sps;
    let snr = 10f64.powf(snr_db / 10.0);
    // Noise power over fs such that the in-band (Carson) SNR is SNR_DB.
    let sigma = ((1.0 / snr) * FS_HZ / scene.bandwidth_hz() / 2.0).sqrt();
    let mut out = Vec::with_capacity(LEAD + n_sig);
    for _ in 0..LEAD {
        out.push(Complex32::new(
            (sigma * rng.gauss()) as f32,
            (sigma * rng.gauss()) as f32,
        ));
    }
    let mut phase = std::f64::consts::TAU * rng.uniform();
    for _ in 0..symbols {
        let level = scene.levels[(rng.next_u64() % scene.levels.len() as u64) as usize];
        let dphi = std::f64::consts::TAU * level * scene.outer_deviation_hz / FS_HZ;
        for _ in 0..sps {
            phase += dphi;
            out.push(Complex32::new(
                (phase.cos() + sigma * rng.gauss()) as f32,
                (phase.sin() + sigma * rng.gauss()) as f32,
            ));
        }
    }
    (out, LEAD)
}

/// What the blocks reached on one draw, as the `docs/07` objects the engine will write.
#[derive(Debug)]
struct EfOutcome {
    resolution: Resolution,
    /// The level count claimed, if any.
    claimed_order: Option<u32>,
    /// The symbol rate claimed, if any.
    claimed_rate_bd: Option<f64>,
    /// What each stage did — printed on failure.
    trace: Vec<String>,
}

fn mean_power(x: &[Complex32]) -> f64 {
    x.iter().map(|c| f64::from(c.norm_sqr())).sum::<f64>() / x.len().max(1) as f64
}

/// Runs the blocks the pipeline runs, **blind**: they get the IQ, the sample rate and the S0
/// channel width. Nothing about the family, alphabet or rate.
fn analyse(iq: &[Complex32], lead: usize, channel_bw_hz: f64) -> EfOutcome {
    let mut trace = Vec::new();
    let (noise, emission) = iq.split_at(lead);
    let ratio = mean_power(emission) / mean_power(noise);
    trace.push(format!("S0 energy ratio {ratio:.2} (floor {ENERGY_RATIO})"));
    if ratio < ENERGY_RATIO {
        return EfOutcome {
            resolution: Resolution {
                kind: ResolutionKind::Unknown,
                deepest_verdict: Some(Verdict::Energy),
                reason: Some(ResolutionReason::NoSignal),
                suspected: None,
                summary: "no energy above the floor".into(),
            },
            claimed_order: None,
            claimed_rate_bd: None,
            trace,
        };
    }
    let mut verdict = Verdict::Energy;
    let mut claimed_order = None;
    let mut claimed_rate_bd = None;
    match structure::measure(emission, FS_HZ, Some(channel_bw_hz)) {
        Ok(s) => {
            trace.push(format!(
                "S1/S2 structure: {:?} at {:.1} Bd ({:.1} clock bits), inner fraction {:.2}, \
                 valley {:.2}",
                s.levels, s.symbol_rate_bd, s.clock_bits, s.inner_fraction, s.valley_ratio
            ));
            claimed_rate_bd = Some(s.symbol_rate_bd);
            claimed_order = s.levels.order();
            verdict = if claimed_order.is_some() {
                Verdict::Clocked
            } else {
                Verdict::Demodulated
            };
            if s.levels.order() == Some(4) {
                verdict = verdict.max(framing_stage(emission, &s, &mut trace));
            } else {
                trace.push(
                    "S4 framing: the catalogue holds no framing for this alphabet in this build"
                        .into(),
                );
            }
        }
        Err(e @ StructureError::NoClockLine { .. }) => {
            trace.push(format!("S1/S2 structure abstained: {e:?}"));
        }
        Err(e) => {
            trace.push(format!("S1/S2 structure could not run: {e:?}"));
        }
    }
    let solved = verdict == Verdict::Solved;
    EfOutcome {
        resolution: Resolution {
            kind: ResolutionKind::Unknown,
            deepest_verdict: Some(verdict),
            // Energy existed and nothing reached a complete candidate: no tie, no budget.
            reason: (!solved).then_some(ResolutionReason::NothingScored),
            suspected: None,
            summary: format!("deepest {verdict:?}; everything tried measured below its floor"),
        },
        claimed_order,
        claimed_rate_bd,
        trace,
    }
}

/// S4/S5 for a measured four-level alphabet: the C4FM demodulator at the **measured** rate, then
/// every framing the catalogue offers.
fn framing_stage(emission: &[Complex32], s: &FmStructure, trace: &mut Vec<String>) -> Verdict {
    let demod = C4fmDemod::new(C4fmConfig {
        symbol_rate_bd: s.symbol_rate_bd,
        ..C4fmConfig::default()
    });
    let dibits = match demod.demodulate(emission, FS_HZ, s.residual_cfo_hz) {
        Ok(sym) => sym.dibits,
        Err(e) => {
            trace.push(format!("S3 dibits: none ({e:?})"));
            return Verdict::Clocked;
        }
    };
    let confirmer = CcConfirmer::default();
    let mut best = Verdict::Clocked;
    for f in CC_FRAMINGS {
        let o = confirmer.scan_framing(f, &dibits);
        trace.push(format!(
            "S4 framing {}: {} trials, {} sync hits, {}/{} CRC-valid",
            f.name(),
            o.trials,
            o.sync_hits,
            o.crc_valid,
            o.crc_checked
        ));
        if o.crc_valid > 0 {
            best = best.max(Verdict::Checked);
        }
    }
    best
}

/// **The expected-failure predicate** (docs/22 §6.2 item 3): the row fails *honestly* — deepest
/// verdict below `framed`, `reason: nothing-scored`, and no over-claim (no level count or symbol
/// rate that contradicts the hidden truth). `Err` carries why the row is RED.
fn honest_failure(o: &EfOutcome, truth: &EfScene) -> Result<(), String> {
    let v = o.resolution.deepest_verdict;
    if v.is_none_or(|v| v >= Verdict::Framed) {
        return Err(format!(
            "deepest verdict {v:?} is not below framed: a row placed past the edge was scored \
             as structured, so a threshold is permissive"
        ));
    }
    if o.resolution.reason != Some(ResolutionReason::NothingScored) {
        return Err(format!(
            "reason {:?}, not nothing-scored: the energy was there and the failure must say so",
            o.resolution.reason
        ));
    }
    let truth_order = truth.levels.len() as u32;
    if let Some(k) = o.claimed_order
        && k != truth_order
    {
        return Err(format!(
            "OVER-CLAIM: {k} levels claimed for a {truth_order}-level emission"
        ));
    }
    if o.claimed_order.is_some()
        && let Some(r) = o.claimed_rate_bd
        && (r / truth.symbol_rate_bd - 1.0).abs() > 0.02
    {
        return Err(format!(
            "OVER-CLAIM: a {truth_order}-level alphabet committed at {r:.1} Bd for a {:.0} Bd \
             emission",
            truth.symbol_rate_bd
        ));
    }
    Ok(())
}

/// Runs one expected-failure family over its **open** seeds and asserts every draw fails
/// honestly.
fn expected_failure_row(scene: &EfScene) {
    let plan = SeedPlan::new(&ef_scene_id(scene.family), EF_SEEDS);
    let seeds = plan.runnable();
    assert!(!seeds.is_empty(), "the hold-out sealed every seed");
    let mut red = Vec::new();
    let mut verdicts = Vec::new();
    for &seed in &seeds {
        let (iq, lead) = generate(scene, seed, SNR_DB, SYMBOLS);
        let o = analyse(&iq, lead, CHANNEL_BW_HZ);
        verdicts.push(format!("{seed}:{:?}", o.resolution.deepest_verdict));
        if let Err(why) = honest_failure(&o, scene) {
            red.push(format!(
                "seed {seed}: {why}\n    {}",
                o.trace.join("\n    ")
            ));
        }
    }
    eprintln!(
        "[{USE_CASES}] EF-{} at {SNR_DB} dB / {SYMBOLS} symbols: {} open draws (sealed {:?} not \
         run{}), verdicts {}",
        scene.family,
        seeds.len(),
        plan.sealed,
        if corpus::unsealed() {
            " -- UNSEALED for M-12"
        } else {
            ""
        },
        verdicts.join(" "),
    );
    assert!(
        red.is_empty(),
        "{USE_CASES}: expected-failure row EF-{} did not fail honestly on {}/{} draws:\n{}",
        scene.family,
        red.len(),
        seeds.len(),
        red.join("\n"),
    );
}

#[test]
fn expected_failure_2fsk_at_6db_and_112_symbols_fails_honestly() {
    expected_failure_row(&EF_2FSK);
}

#[test]
fn expected_failure_c4fm_at_6db_and_112_symbols_fails_honestly() {
    expected_failure_row(&EF_C4FM);
}

#[test]
fn expected_failure_msk_at_6db_and_112_symbols_fails_honestly() {
    expected_failure_row(&EF_MSK);
}

/// **RED without the fix.** The predicate the rows assert with must reject each dishonest shape a
/// permissive engine would produce — otherwise the rows pass for the same reason a suite with no
/// expected-failure rows does.
#[test]
fn expected_failure_predicate_is_red_on_every_dishonest_outcome() {
    let outcome =
        |v: Verdict, r: Option<ResolutionReason>, k: Option<u32>, rate: Option<f64>| EfOutcome {
            resolution: Resolution {
                kind: ResolutionKind::Unknown,
                deepest_verdict: Some(v),
                reason: r,
                suspected: None,
                summary: String::new(),
            },
            claimed_order: k,
            claimed_rate_bd: rate,
            trace: Vec::new(),
        };
    let ns = Some(ResolutionReason::NothingScored);
    // The honest shapes pass.
    assert!(honest_failure(&outcome(Verdict::Energy, ns, None, None), &EF_2FSK).is_ok());
    assert!(
        honest_failure(
            &outcome(Verdict::Clocked, ns, Some(2), Some(4800.0)),
            &EF_2FSK
        )
        .is_ok()
    );
    // Each dishonest one is RED.
    for (what, o) in [
        (
            "framed",
            outcome(Verdict::Framed, ns, Some(2), Some(4800.0)),
        ),
        (
            "checked",
            outcome(Verdict::Checked, ns, Some(4), Some(4800.0)),
        ),
        ("solved", outcome(Verdict::Solved, None, Some(2), None)),
        (
            "tied",
            outcome(Verdict::Clocked, Some(ResolutionReason::Tied), None, None),
        ),
        (
            "no-signal",
            outcome(
                Verdict::Energy,
                Some(ResolutionReason::NoSignal),
                None,
                None,
            ),
        ),
        (
            "wrong order",
            outcome(Verdict::Clocked, ns, Some(4), Some(4800.0)),
        ),
        (
            "wrong rate",
            outcome(Verdict::Clocked, ns, Some(2), Some(9600.0)),
        ),
    ] {
        assert!(
            honest_failure(&o, &EF_2FSK).is_err(),
            "{USE_CASES}: the predicate accepted a {what} outcome"
        );
    }
}

/// **The control that keeps the expected-failure rows meaningful.** A row that fails honestly
/// because its generator emits nothing structured would pass for the wrong reason, exactly as a
/// false-confirm suite with no recall control is passed by never confirming. The same generator,
/// the same blocks and the same seed well inside the edge — 20 dB, 4096 symbols — must reach
/// `clocked` with the right level count and rate.
#[test]
fn expected_failure_rows_have_a_control_inside_the_edge_that_clocks() {
    for scene in [&EF_2FSK, &EF_C4FM, &EF_MSK] {
        let seed = SeedPlan::new(&ef_scene_id(scene.family), EF_SEEDS).open[0];
        let (iq, lead) = generate(scene, seed, 20.0, 4096);
        let o = analyse(&iq, lead, CHANNEL_BW_HZ);
        eprintln!(
            "[{USE_CASES}] control {} at 20 dB / 4096 symbols: {:?}\n    {}",
            scene.family,
            o.resolution.deepest_verdict,
            o.trace.join("\n    "),
        );
        assert_eq!(
            o.resolution.deepest_verdict,
            Some(Verdict::Clocked),
            "{USE_CASES}: control {}: the generator or the blocks are broken, so the \
             expected-failure row's honest failure proves nothing",
            scene.family,
        );
        assert_eq!(o.claimed_order, Some(scene.levels.len() as u32));
        let r = o.claimed_rate_bd.unwrap();
        assert!((r / scene.symbol_rate_bd - 1.0).abs() < 0.02, "rate {r}");
    }
}
