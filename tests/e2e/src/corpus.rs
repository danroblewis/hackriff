//! The MAUTO corpus's **coverage manifest** and **sealed hold-out** (T-627; docs/22 §4.5, §6.2,
//! §6.4).
//!
//! A fixture corpus that is merely *present* passes by covering what someone happened to think
//! of, and its gaps are invisible from inside it. This module makes the gap observable in two
//! ways, neither of which is a scene:
//!
//! 1. **The coverage manifest** ([`Manifest`]). The *cell space* is the artefact, not the cell
//!    contents: every cell of the declared axis product ([`docs22_axes`], [`DOCS22_PLANES`]) gets
//!    exactly one [`Mark`] — [`Mark::Populated`] (with the count of open jobs that ran it),
//!    [`Mark::Unreachable`] (with a reason and a ticket), or [`Mark::Unmarked`]. The third mark is
//!    **a failure of the manifest, not of the engine**: a cell nobody thought about. The same
//!    discipline as the canvas coverage map — `Coverage::Unobserved` is not `quiet`, and an honest
//!    empty cell beats a silently absent one.
//! 2. **The sealed hold-out** ([`sealed`]). 20 % of every generated scene family, chosen by
//!    `sha256(scene_id ‖ seed) mod 5 == 0` — a rule, not a judgement — is generated and truthed
//!    and then **not run** until the M-12 review ([`UNSEAL_ENV`]). If sealed-set performance then
//!    differs materially from the open set, the open set was overfit. Because the sealer is a
//!    hash, the process that built the corpus cannot choose what it seals.
//!
//! Everything here is pure and deterministic, so the rendered manifest ([`Manifest::render`]) is
//! byte-identical run to run and can be diffed against a committed copy.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Environment variable that **unseals** the hold-out. Its value must be exactly
/// [`UNSEAL_VALUE`]; anything else leaves the sealed seeds unrun.
pub const UNSEAL_ENV: &str = "HK_MAUTO_UNSEAL";

/// The only value of [`UNSEAL_ENV`] that unseals: the review the hold-out is reserved for.
pub const UNSEAL_VALUE: &str = "M-12";

/// The hold-out modulus: one seed in five is sealed.
pub const HOLDOUT_MODULUS: u32 = 5;

/// Whether `(scene_id, seed)` belongs to the **sealed hold-out** (docs/22 §4.5).
///
/// The rule, byte for byte: `sha256(utf8(scene_id) ‖ be_u64(seed))`, read as a big-endian
/// 256-bit integer, `mod 5 == 0`. The seed is fixed-width so the concatenation is unambiguous
/// (`"a" ‖ 12` and `"a1" ‖ 2` are different inputs).
pub fn sealed(scene_id: &str, seed: u64) -> bool {
    holdout_residue(scene_id, seed) == 0
}

/// `sha256(scene_id ‖ seed) mod 5` — exposed so the rule's test vectors can pin the residue, not
/// only the boolean.
pub fn holdout_residue(scene_id: &str, seed: u64) -> u32 {
    let mut h = Sha256::new();
    h.update(scene_id.as_bytes());
    h.update(seed.to_be_bytes());
    let digest = h.finalize();
    digest
        .iter()
        .fold(0u32, |r, &b| (r * 256 + u32::from(b)) % HOLDOUT_MODULUS)
}

/// Whether this process may run sealed seeds: only when [`UNSEAL_ENV`] is [`UNSEAL_VALUE`].
pub fn unsealed() -> bool {
    std::env::var(UNSEAL_ENV).is_ok_and(|v| v == UNSEAL_VALUE)
}

/// A scene family's seeds, split by the hold-out rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedPlan {
    /// The scene family id the rule hashed.
    pub scene_id: String,
    /// Seeds the open suite runs.
    pub open: Vec<u64>,
    /// Seeds generated and truthed but not run until M-12.
    pub sealed: Vec<u64>,
}

impl SeedPlan {
    /// Splits `seeds` of `scene_id` by [`sealed`], preserving order.
    pub fn new(scene_id: &str, seeds: impl IntoIterator<Item = u64>) -> Self {
        let (sealed_seeds, open): (Vec<u64>, Vec<u64>) =
            seeds.into_iter().partition(|&s| sealed(scene_id, s));
        Self {
            scene_id: scene_id.to_owned(),
            open,
            sealed: sealed_seeds,
        }
    }

    /// The seeds this process runs: the open set, plus the sealed set only when [`unsealed`].
    pub fn runnable(&self) -> Vec<u64> {
        let mut v = self.open.clone();
        if unsealed() {
            v.extend(&self.sealed);
        }
        v
    }
}

/// One axis of docs/22 §2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Axis {
    /// `A1`…`A8`.
    pub id: &'static str,
    /// What it varies.
    pub name: &'static str,
    /// Its declared levels, in docs/22 §2's order.
    pub levels: &'static [&'static str],
}

/// The declared axes, **exactly as docs/22 §2 lists them**. A level added there is added here,
/// and every plane that crosses the axis grows cells that must then be marked.
pub fn docs22_axes() -> Vec<Axis> {
    vec![
        Axis {
            id: "A1",
            name: "modulation family",
            levels: &[
                "2fsk",
                "4fsk-c4fm",
                "ook-ask",
                "msk",
                "css-lora",
                "fm-voice",
                "am-voice",
                "cw",
                "ofdm",
                "dsss",
                "16qam",
                "thermal-noise",
            ],
        },
        Axis {
            id: "A2",
            name: "SNR in the emission's own bandwidth",
            levels: &["6dB", "8dB", "10dB", "20dB"],
        },
        Axis {
            id: "A3",
            name: "ADC fill, noise sigma in LSB",
            levels: &["0.21", "0.6", "2.0", "23", "43", "77-clipped"],
        },
        Axis {
            id: "A4",
            name: "support: symbols per window, or burst",
            levels: &["112", "512", "4096", "burst"],
        },
        Axis {
            id: "A5",
            name: "catalogue membership",
            levels: &[
                "template-fixed",
                "in-catalogue-searched",
                "out-of-catalogue",
                "structureless",
            ],
        },
        Axis {
            id: "A6",
            name: "CFO",
            levels: &["0", "+0.2bw", "-0.2bw"],
        },
        Axis {
            id: "A7",
            name: "check",
            levels: &[
                "crc8-fixed",
                "crc16-fixed",
                "crc24-fixed",
                "crc32-fixed",
                "crc-searched",
                "crc-random-poly",
                "bch31-21",
                "none",
                "constant-payload",
            ],
        },
        Axis {
            id: "A8",
            name: "templates",
            levels: &["on", "off"],
        },
    ]
}

/// The **expected-failure** plane's name (docs/22 §6.2 item 3): one cell per family, the row
/// placed deliberately past the edge (6 dB at 112 symbols) and asserted to fail honestly.
pub const EXPECTED_FAILURE_PLANE: &str = "EF";

/// The declared planes of the manifest.
///
/// docs/22 §2 is a **stratified sample of an axis product**, not a full crossing (~10⁵ jobs), so
/// the manifest declares the crossings the corpus is argued over rather than the full product:
/// the recall grid the bars are indexed on (family × SNR × support), family against each other
/// axis, and the expected-failure plane. Every plane is anchored on A1, because every claim in
/// docs/22 §6.1 is per family.
pub const DOCS22_PLANES: &[(&str, &[&str])] = &[
    ("A1xA2xA4", &["A1", "A2", "A4"]),
    ("A1xA3", &["A1", "A3"]),
    ("A1xA5", &["A1", "A5"]),
    ("A1xA6", &["A1", "A6"]),
    ("A1xA7", &["A1", "A7"]),
    ("A1xA8", &["A1", "A8"]),
    (EXPECTED_FAILURE_PLANE, &["A1"]),
];

/// One cell: a plane and one level per axis in it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell {
    /// The plane's name.
    pub plane: String,
    /// `(axis id, level)` in the plane's axis order.
    pub levels: Vec<(String, String)>,
}

impl Cell {
    /// A cell of `plane` at `levels` (`&[("A1", "2fsk"), …]`).
    pub fn new(plane: &str, levels: &[(&str, &str)]) -> Self {
        Self {
            plane: plane.to_owned(),
            levels: levels
                .iter()
                .map(|(a, l)| ((*a).to_owned(), (*l).to_owned()))
                .collect(),
        }
    }

    /// The level on `axis`, when the plane crosses it.
    pub fn level(&self, axis: &str) -> Option<&str> {
        self.levels
            .iter()
            .find(|(a, _)| a == axis)
            .map(|(_, l)| l.as_str())
    }

    /// `A1=2fsk A2=6dB A4=112`.
    pub fn key(&self) -> String {
        self.levels
            .iter()
            .map(|(a, l)| format!("{a}={l}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Every cell of every declared plane, in a stable order.
pub fn declared_cells(axes: &[Axis], planes: &[(&str, &[&str])]) -> Vec<Cell> {
    let mut out = Vec::new();
    for (plane, ids) in planes {
        let dims: Vec<&Axis> = ids
            .iter()
            .map(|id| {
                axes.iter()
                    .find(|a| a.id == *id)
                    .unwrap_or_else(|| panic!("plane {plane} names undeclared axis {id}"))
            })
            .collect();
        let mut idx = vec![0usize; dims.len()];
        loop {
            out.push(Cell {
                plane: (*plane).to_owned(),
                levels: dims
                    .iter()
                    .zip(&idx)
                    .map(|(a, &i)| (a.id.to_owned(), a.levels[i].to_owned()))
                    .collect(),
            });
            // Odometer, last axis fastest.
            let mut k = dims.len();
            loop {
                if k == 0 {
                    break;
                }
                k -= 1;
                idx[k] += 1;
                if idx[k] < dims[k].levels.len() {
                    break;
                }
                idx[k] = 0;
            }
            if idx.iter().all(|&i| i == 0) {
                break;
            }
        }
    }
    out
}

/// Who owns the gap a declaration names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ticket {
    /// An open ticket on the board.
    Filed(&'static str),
    /// **No ticket exists.** Printed as `UNFILED` so the absence of an owner is itself a mark,
    /// never hidden behind a stale or wrong id.
    Unfiled,
}

impl Ticket {
    fn render(&self) -> &'static str {
        match self {
            Ticket::Filed(t) => t,
            Ticket::Unfiled => "UNFILED",
        }
    }
}

/// A **declared unreachable** region: cells somebody thought about and could not fill.
///
/// A declaration always names its family — there is no catch-all, because a wildcard that marks
/// everything is exactly the silent absence the manifest exists to prevent. It applies to a cell
/// when the cell's A1 level is `family`, the cell is on one of `planes` (all planes when empty),
/// and for every `(axis, level)` in `when` the cell either crosses that axis at that level or
/// does not cross it at all… except that a `when` constraint on an axis the plane lacks makes the
/// declaration **not apply** — so a narrow declaration can never leak into a wider plane.
#[derive(Clone, Debug)]
pub struct Unreachable {
    /// The family (A1 level) — mandatory.
    pub family: &'static str,
    /// Planes it covers; empty = all.
    pub planes: &'static [&'static str],
    /// Further level constraints.
    pub when: &'static [(&'static str, &'static str)],
    /// Why it cannot be filled today.
    pub reason: &'static str,
    /// Who owns filling it.
    pub ticket: Ticket,
}

impl Unreachable {
    /// Whether this declaration marks `cell`.
    pub fn applies(&self, cell: &Cell) -> bool {
        cell.level("A1") == Some(self.family)
            && (self.planes.is_empty() || self.planes.contains(&cell.plane.as_str()))
            && self
                .when
                .iter()
                .all(|(axis, level)| cell.level(axis) == Some(*level))
    }
}

/// A **populated** row: a test in the suite that runs the open seeds of one scene family and
/// thereby fills cells.
#[derive(Clone, Debug)]
pub struct Row {
    /// Row id (`EF-2fsk`, `P-087`).
    pub id: &'static str,
    /// The tests (`module::fn`, as the test binary lists them) that run it.
    pub tests: &'static [&'static str],
    /// The hold-out split of its seeds.
    pub seeds: SeedPlan,
    /// `Some(reason)` for a row whose seeds are **fixed and pre-date the seal** (a single
    /// hand-built acceptance scene, not draws from a generated family): it runs every seed in the
    /// open suite, and the manifest says so in words rather than reporting the rule's split for
    /// seeds that are in fact run.
    pub fixed_seeds: Option<&'static str>,
    /// The cells it fills.
    pub cells: Vec<Cell>,
}

/// One cell's mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    /// Jobs ran: `n` open jobs, `sealed` held out, from these rows.
    Populated {
        /// Open jobs.
        n: usize,
        /// Sealed jobs (generated, truthed, not run until M-12).
        sealed: usize,
        /// Row ids.
        rows: Vec<&'static str>,
        /// Why some of those jobs are fixed seeds exempt from the seal, when any are.
        fixed: Vec<&'static str>,
    },
    /// Somebody thought about it and could not fill it.
    Unreachable {
        /// Why.
        reason: &'static str,
        /// Owner.
        ticket: Ticket,
    },
    /// **Nobody thought about it.** A failure of the manifest.
    Unmarked,
}

/// The coverage manifest.
#[derive(Clone, Debug)]
pub struct Manifest {
    /// Every declared cell and its mark, in declaration order.
    pub cells: Vec<(Cell, Mark)>,
}

impl Manifest {
    /// Marks every declared cell: populated by `rows` first (a cell that ran is populated
    /// whatever is declared about it), else the **first** matching declaration, else unmarked.
    ///
    /// # Panics
    /// When a row names a cell outside the declared product — a row cannot populate a cell the
    /// manifest does not declare, or the manifest would under-report what the corpus is.
    pub fn build(declared: Vec<Cell>, rows: &[Row], decls: &[Unreachable]) -> Self {
        type Acc = (usize, usize, Vec<&'static str>, Vec<&'static str>);
        let mut pop: BTreeMap<Cell, Acc> = BTreeMap::new();
        for row in rows {
            for cell in &row.cells {
                assert!(
                    declared.contains(cell),
                    "row {} populates {} [{}], which is not a declared cell",
                    row.id,
                    cell.plane,
                    cell.key(),
                );
                let e = pop.entry(cell.clone()).or_default();
                match row.fixed_seeds {
                    Some(why) => {
                        e.0 += row.seeds.open.len() + row.seeds.sealed.len();
                        e.3.push(why);
                    }
                    None => {
                        e.0 += row.seeds.open.len();
                        e.1 += row.seeds.sealed.len();
                    }
                }
                e.2.push(row.id);
            }
        }
        let cells = declared
            .into_iter()
            .map(|cell| {
                let mark = if let Some((n, sealed, rows, fixed)) = pop.remove(&cell) {
                    Mark::Populated {
                        n,
                        sealed,
                        rows,
                        fixed,
                    }
                } else if let Some(d) = decls.iter().find(|d| d.applies(&cell)) {
                    Mark::Unreachable {
                        reason: d.reason,
                        ticket: d.ticket.clone(),
                    }
                } else {
                    Mark::Unmarked
                };
                (cell, mark)
            })
            .collect();
        Self { cells }
    }

    /// Cells nobody thought about.
    pub fn unmarked(&self) -> Vec<&Cell> {
        self.cells
            .iter()
            .filter(|(_, m)| *m == Mark::Unmarked)
            .map(|(c, _)| c)
            .collect()
    }

    /// `(populated, unreachable, unfiled-unreachable, unmarked)` cell counts.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut c = (0, 0, 0, 0);
        for (_, m) in &self.cells {
            match m {
                Mark::Populated { .. } => c.0 += 1,
                Mark::Unreachable { ticket, .. } => {
                    c.1 += 1;
                    if *ticket == Ticket::Unfiled {
                        c.2 += 1;
                    }
                }
                Mark::Unmarked => c.3 += 1,
            }
        }
        c
    }

    /// The manifest as stable, line-oriented text: a header with the counts, then one line per
    /// cell. Byte-identical for identical inputs, so two runs diff cleanly.
    pub fn render(&self) -> String {
        let (p, u, unfiled, un) = self.counts();
        let mut s = String::new();
        let _ = writeln!(
            s,
            "# MAUTO coverage manifest (T-627; docs/22 §6.4). Generated by \
             acceptance_mauto::mauto_corpus -- do not edit by hand; re-bless with \
             HK_BLESS_MANIFEST=1."
        );
        let _ = writeln!(
            s,
            "# Read this BEFORE any pass rate. unmarked = a cell nobody thought about = a failure \
             of the manifest."
        );
        let _ = writeln!(
            s,
            "# cells {}: populated {p}, declared-unreachable {u} (of which UNFILED {unfiled}), \
             unmarked {un}",
            self.cells.len()
        );
        let _ = writeln!(
            s,
            "# hold-out: sha256(scene_id || be_u64(seed)) mod {HOLDOUT_MODULUS} == 0 is sealed \
             until {UNSEAL_VALUE} ({UNSEAL_ENV})"
        );
        for (cell, mark) in &self.cells {
            let m = match mark {
                Mark::Populated {
                    n,
                    sealed,
                    rows,
                    fixed,
                } => {
                    let mut m = format!("populated n={n} sealed={sealed} rows={}", rows.join(","));
                    for why in fixed {
                        let _ = write!(m, " [fixed seed, not sealed: {why}]");
                    }
                    m
                }
                Mark::Unreachable { reason, ticket } => {
                    format!("unreachable {}: {reason}", ticket.render())
                }
                Mark::Unmarked => "UNMARKED".to_owned(),
            };
            let _ = writeln!(s, "{:<9} {:<48} {m}", cell.plane, cell.key());
        }
        s
    }
}

/// The board status of `ticket` in the text of `docs/tasks.yaml`, read without a YAML parser:
/// the first `status:` line inside the `- id: <ticket>` block. `None` when the ticket is absent.
pub fn ticket_status(tasks_yaml: &str, ticket: &str) -> Option<String> {
    let header = format!("- id: {ticket}");
    let mut in_block = false;
    for line in tasks_yaml.lines() {
        let t = line.trim_start();
        if t.starts_with("- id: ") {
            if in_block {
                return None;
            }
            in_block = t.trim_end() == header;
            continue;
        }
        if in_block && let Some(rest) = t.strip_prefix("status:") {
            return Some(rest.trim().trim_matches(['"', '\'']).to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_cells_are_the_full_crossing_of_each_plane() {
        let axes = docs22_axes();
        let cells = declared_cells(&axes, DOCS22_PLANES);
        let want: usize = DOCS22_PLANES
            .iter()
            .map(|(_, ids)| {
                ids.iter()
                    .map(|id| axes.iter().find(|a| a.id == *id).unwrap().levels.len())
                    .product::<usize>()
            })
            .sum();
        assert_eq!(cells.len(), want);
        let mut dedup = cells.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(dedup.len(), cells.len(), "no cell is declared twice");
    }

    #[test]
    fn an_unthought_cell_is_unmarked_not_quiet() {
        let axes = docs22_axes();
        let declared = declared_cells(&axes, &[("EF", &["A1"])]);
        let decls = [Unreachable {
            family: "ook-ask",
            planes: &[],
            when: &[],
            reason: "no generator",
            ticket: Ticket::Unfiled,
        }];
        let m = Manifest::build(declared, &[], &decls);
        let unmarked = m.unmarked();
        assert_eq!(unmarked.len(), 11, "only ook-ask was thought about");
        assert!(m.render().contains("UNMARKED"));
        assert!(m.render().contains("unreachable UNFILED: no generator"));
    }

    #[test]
    fn a_when_constraint_on_an_axis_the_plane_lacks_does_not_apply() {
        let d = Unreachable {
            family: "2fsk",
            planes: &[],
            when: &[("A7", "none")],
            reason: "x",
            ticket: Ticket::Unfiled,
        };
        assert!(!d.applies(&Cell::new("EF", &[("A1", "2fsk")])));
        assert!(d.applies(&Cell::new("A1xA7", &[("A1", "2fsk"), ("A7", "none")])));
        assert!(!d.applies(&Cell::new("A1xA7", &[("A1", "msk"), ("A7", "none")])));
    }

    #[test]
    fn ticket_status_reads_the_block_it_names() {
        let y = "tasks:\n  - id: T-1\n    title: a\n    status: done\n  - id: T-10\n    status: todo\n  - id: T-2\n    title: no status\n  - id: T-3\n    status: 'blocked'\n";
        assert_eq!(ticket_status(y, "T-1").as_deref(), Some("done"));
        assert_eq!(ticket_status(y, "T-10").as_deref(), Some("todo"));
        assert_eq!(ticket_status(y, "T-2"), None);
        assert_eq!(ticket_status(y, "T-3").as_deref(), Some("blocked"));
        assert_eq!(ticket_status(y, "T-9"), None);
    }
}
