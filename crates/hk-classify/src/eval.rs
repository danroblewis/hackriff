//! Per-SNR, per-family accuracy reporting (ADR-0016 §7).
//!
//! **A single SNR-averaged number is never reported alone** (C15 card pitfall: averaging hides the
//! floor where a family stops working), so every row here is `source × family × SNR bin`.
//!
//! Truth enters only through [`EvalReport::record`]'s `truth` argument, which the *test* holds —
//! the classifier is never told what it is looking at (the blind rule, docs/10 §3.2). A `None`
//! truth is a held-out generator, whose right answer is `unknown`.
//!
//! T-213 lands the project-wide harness (`hk_model::classify::eval::EvalReport`, the Python AMC
//! grid and the OTA decode-label loader). This type is deliberately small and self-contained so it
//! can be dropped when that arrives; the rows it produces are the same slicing.

use std::collections::BTreeMap;

use hk_model::classify::{Classification, UNKNOWN};

/// One `source × family × SNR bin` cell.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    /// Snippets in the cell.
    pub n: usize,
    /// Top-1 posterior equals the truth family.
    pub top1: usize,
    /// Truth family is in the top 2.
    pub top2: usize,
    /// Reported `unknown`.
    pub unknown: usize,
    /// Reported a family, and it was the wrong one (abstaining is not wrong).
    pub wrong: usize,
}

impl Cell {
    /// Top-1 accuracy over the cell.
    pub fn top1_rate(&self) -> f64 {
        ratio(self.top1, self.n)
    }

    /// Top-2 accuracy over the cell.
    pub fn top2_rate(&self) -> f64 {
        ratio(self.top2, self.n)
    }

    /// Share that abstained.
    pub fn unknown_rate(&self) -> f64 {
        ratio(self.unknown, self.n)
    }

    /// Share that named the wrong family.
    pub fn wrong_rate(&self) -> f64 {
        ratio(self.wrong, self.n)
    }
}

fn ratio(a: usize, b: usize) -> f64 {
    if b == 0 { 0.0 } else { a as f64 / b as f64 }
}

/// Key of one cell: source, truth family (`unknown` for held-out generators), SNR bin in dB.
type Key = (String, String, i64);

/// Key of one class-level cell: source, truth family, truth class, SNR bin in dB.
type ClassKey = (String, String, String, i64);

/// A blind evaluation report.
#[derive(Clone, Debug, Default)]
pub struct EvalReport {
    cells: BTreeMap<Key, Cell>,
    class_cells: BTreeMap<ClassKey, Cell>,
    confusion: BTreeMap<(String, String), usize>,
    /// SNR bin width, dB.
    pub bin_db: f64,
}

impl EvalReport {
    /// A report binning SNR into `bin_db`-wide bins.
    pub fn new(bin_db: f64) -> Self {
        Self {
            cells: BTreeMap::new(),
            class_cells: BTreeMap::new(),
            confusion: BTreeMap::new(),
            bin_db: if bin_db > 0.0 { bin_db } else { 5.0 },
        }
    }

    fn bin_of(&self, snr_db: f64) -> i64 {
        (snr_db / self.bin_db).floor() as i64 * self.bin_db as i64
    }

    /// Records one classification. `truth` is the expected `hk-mod@1` family, or `None` for a
    /// held-out generator (expected outcome: `unknown`).
    pub fn record(&mut self, source: &str, truth: Option<&str>, snr_db: f64, c: &Classification) {
        let expected = truth.unwrap_or(UNKNOWN);
        let bin = self.bin_of(snr_db);
        let cell = self
            .cells
            .entry((source.to_owned(), expected.to_owned(), bin))
            .or_default();
        cell.n += 1;
        let top: Vec<String> = c.top(2).into_iter().map(|lp| lp.label).collect();
        let called_unknown = c.family == UNKNOWN;
        if called_unknown {
            cell.unknown += 1;
        }
        if c.family == expected {
            cell.top1 += 1;
        } else if !called_unknown && truth.is_some() {
            cell.wrong += 1;
        } else if !called_unknown && truth.is_none() {
            // A held-out generator given a family is a false "known".
            cell.wrong += 1;
        }
        if top.iter().any(|l| l == expected) {
            cell.top2 += 1;
        }
        *self
            .confusion
            .entry((expected.to_owned(), c.family.clone()))
            .or_default() += 1;
    }

    /// [`Self::record`], plus a within-family class truth folded into a second, class-level table
    /// (`source × family × class × SNR bin`). Pass `class_truth: None` when the truth has no
    /// meaningful class (a held-out generator, or a family this taxonomy gives none). The class
    /// call only counts once the classifier reports one at all: `c.class == None` is an
    /// abstention at the class level, distinct from (and possible even when) the family call is
    /// correct — a family can pass its gate while its within-family class stays unresolved.
    pub fn record_with_class(
        &mut self,
        source: &str,
        family_truth: Option<&str>,
        class_truth: Option<&str>,
        snr_db: f64,
        c: &Classification,
    ) {
        self.record(source, family_truth, snr_db, c);
        let Some(expected_class) = class_truth else {
            return;
        };
        let family = family_truth.unwrap_or(UNKNOWN).to_owned();
        let bin = self.bin_of(snr_db);
        let cell = self
            .class_cells
            .entry((source.to_owned(), family, expected_class.to_owned(), bin))
            .or_default();
        cell.n += 1;
        match &c.class {
            Some(call) if call.label == expected_class => {
                cell.top1 += 1;
                cell.top2 += 1;
            }
            Some(call) => {
                cell.wrong += 1;
                let mut dist = call.dist.clone();
                dist.sort_by(|a, b| b.p.total_cmp(&a.p));
                if dist.iter().take(2).any(|lp| lp.label == expected_class) {
                    cell.top2 += 1;
                }
            }
            None => cell.unknown += 1,
        }
    }

    /// Every family-level cell, in `(source, family, bin)` order.
    pub fn cells(&self) -> impl Iterator<Item = (&str, &str, i64, &Cell)> {
        self.cells
            .iter()
            .map(|((s, f, b), c)| (s.as_str(), f.as_str(), *b, c))
    }

    /// Every class-level cell, in `(source, family, class, bin)` order.
    pub fn class_cells(&self) -> impl Iterator<Item = (&str, &str, &str, i64, &Cell)> {
        self.class_cells
            .iter()
            .map(|((s, f, cl, b), c)| (s.as_str(), f.as_str(), cl.as_str(), *b, c))
    }

    /// The cells of one source at or above `min_snr_db`, summed.
    pub fn total(&self, source: Option<&str>, min_snr_db: f64) -> Cell {
        let mut t = Cell::default();
        for ((s, _, bin), c) in &self.cells {
            if source.is_some_and(|want| want != s) || (*bin as f64) < min_snr_db {
                continue;
            }
            t.n += c.n;
            t.top1 += c.top1;
            t.top2 += c.top2;
            t.unknown += c.unknown;
            t.wrong += c.wrong;
        }
        t
    }

    /// One family's cells at or above `min_snr_db`, summed.
    pub fn family(&self, family: &str, min_snr_db: f64) -> Cell {
        let mut t = Cell::default();
        for ((_, f, bin), c) in &self.cells {
            if f != family || (*bin as f64) < min_snr_db {
                continue;
            }
            t.n += c.n;
            t.top1 += c.top1;
            t.top2 += c.top2;
            t.unknown += c.unknown;
            t.wrong += c.wrong;
        }
        t
    }

    /// The worst per-bin wrong-label rate over every cell with at least `min_n` samples (the
    /// ADR-0016 §7 "any bin" floor).
    pub fn worst_bin_wrong_rate(&self, min_n: usize) -> f64 {
        self.cells
            .values()
            .filter(|c| c.n >= min_n)
            .map(Cell::wrong_rate)
            .fold(0.0, f64::max)
    }

    /// A Markdown table of every cell, plus the confusion pairs that occurred.
    pub fn markdown(&self) -> String {
        let mut out = String::from(
            "| source | truth | SNR bin dB | n | top-1 | top-2 | unknown | wrong |\n\
             |---|---|---:|---:|---:|---:|---:|---:|\n",
        );
        for ((s, f, bin), c) in &self.cells {
            out.push_str(&format!(
                "| {s} | {f} | {bin} | {} | {:.2} | {:.2} | {:.2} | {:.2} |\n",
                c.n,
                c.top1_rate(),
                c.top2_rate(),
                c.unknown_rate(),
                c.wrong_rate()
            ));
        }
        out.push_str("\nconfusion (truth → called, count > 0):\n");
        for ((truth, called), n) in &self.confusion {
            if truth != called {
                out.push_str(&format!("  {truth} → {called}: {n}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classifier::{Classifier, ClassifyRequest};
    use crate::synth::{Class, SynthConfig, generate};

    /// A real classification of `class` at `snr_db`: the report is exercised on rows the
    /// classifier actually produced, not on hand-built ones.
    fn classified(class: Class, snr_db: f64) -> Classification {
        let s = generate(class, &SynthConfig::new(snr_db, 1_000_777));
        let mut req = ClassifyRequest::new(
            &s.samples,
            s.sample_rate_hz,
            hk_model::Timestamp::UNIX_EPOCH,
        );
        req.obw_hz = Some(s.obw_hz);
        req.snr_db = Some(snr_db);
        Classifier::new().classify(&req)
    }

    #[test]
    fn rows_are_per_family_and_per_snr_bin_and_abstention_is_not_wrong() {
        let mut r = EvalReport::new(5.0);
        // A high-SNR FSK burst (a family call) and a below-gate one (an abstention).
        let fsk = classified(Class::Fsk2, 25.0);
        assert_eq!(fsk.family, "fsk", "{:?}", fsk.top(3));
        let unknown = classified(Class::Fsk2, 2.0);
        assert_eq!(unknown.family, UNKNOWN);
        r.record("synthetic", Some("fsk"), 22.0, &fsk);
        r.record("synthetic", Some("fsk"), 23.0, &unknown);
        r.record("synthetic", Some("analog"), 22.0, &fsk);
        r.record("synthetic", None, 22.0, &unknown);

        let cell = r.family("fsk", 20.0);
        assert_eq!((cell.n, cell.top1, cell.unknown, cell.wrong), (2, 1, 1, 0));
        let analog = r.family("analog", 20.0);
        assert_eq!((analog.n, analog.wrong), (1, 1));
        // The held-out row abstained: not a false known.
        assert_eq!(r.family(UNKNOWN, 20.0).wrong, 0);
        // Binning: 22 and 23 dB share the 20 dB bin.
        assert_eq!(r.cells().filter(|(_, f, _, _)| *f == "fsk").count(), 1);
        assert!(r.markdown().contains("| synthetic | fsk | 20 |"));
        assert!(r.markdown().contains("analog → fsk"));
    }
}
