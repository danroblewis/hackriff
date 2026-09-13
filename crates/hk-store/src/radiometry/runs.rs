//! The floor product's per-frame state log: flags, gain setting and calibration per time span,
//! run-length encoded, because tiles keep only a per-tile digest.
//!
//! Frames are folded into **cell records** (one per level-0 time cell and receiver state; flags
//! OR'ed within the cell), and consecutive cell records with equal state and flags merge into a
//! **run**, capped at `max_run`. So flags resolve to one level-0 time cell, a gain or calibration
//! change inside a cell yields two runs (a mixed cell), and steady operation writes one line per
//! `max_run`. Closed runs are appended to `runs.tsv` (one text line each); a crash loses at most
//! the open run (≤ `max_run`, matching the pyramid checkpoint interval). Retention is not tied to
//! the pyramid budget yet (a few MB/day worst case).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use hk_dsp::radiometry::{FloorFlags, same_gain};
use hk_model::{CalibrationStateId, FreqRange, GainSetting};

use crate::history::StoreError;

const HEADER: &str = "# hackriff floor runs v1: t0_ns t1_ns f_lo_hz f_hi_hz flags lna_db vga_db amp cal_id cal_sigma_db";

/// One span of constant receiver state and flags.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FloorRun {
    /// Start, ns since the epoch.
    pub t0_ns: i64,
    /// End (exclusive), ns.
    pub t1_ns: i64,
    /// Lowest bin-centre frequency, Hz.
    pub f_lo_hz: f64,
    /// Highest bin-centre frequency, Hz.
    pub f_hi_hz: f64,
    /// Flags (OR over the run's frames within each level-0 time cell).
    pub flags: FloorFlags,
    /// Gain setting.
    pub gain: GainSetting,
    /// Calibration applied (`None`: uncalibrated, folded into the dBFS pyramid).
    pub calibration: Option<CalibrationStateId>,
    /// Calibration standard uncertainty, dB (0 when uncalibrated).
    pub cal_uncertainty_db: f64,
}

impl FloorRun {
    fn same_state(&self, o: &FloorRun) -> bool {
        self.f_lo_hz == o.f_lo_hz
            && self.f_hi_hz == o.f_hi_hz
            && same_gain(&self.gain, &o.gain)
            && self.calibration == o.calibration
            && self.cal_uncertainty_db == o.cal_uncertainty_db
    }

    /// The run covers some of `[t0, t1)` and of `freq`.
    pub fn overlaps(&self, t0_ns: i64, t1_ns: i64, freq: &FreqRange) -> bool {
        self.t0_ns < t1_ns
            && self.t1_ns > t0_ns
            && self.f_lo_hz <= freq.hi_hz
            && self.f_hi_hz >= freq.lo_hz
    }

    fn to_line(self) -> String {
        let cal = self
            .calibration
            .map_or_else(|| "-".to_owned(), |c| c.to_string());
        format!(
            "{} {} {} {} {} {} {} {} {} {}\n",
            self.t0_ns,
            self.t1_ns,
            self.f_lo_hz,
            self.f_hi_hz,
            self.flags.bits(),
            self.gain.lna_db,
            self.gain.vga_db,
            u8::from(self.gain.amp_on),
            cal,
            self.cal_uncertainty_db
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split_ascii_whitespace().collect();
        if f.len() != 10 {
            return None;
        }
        Some(Self {
            t0_ns: f[0].parse().ok()?,
            t1_ns: f[1].parse().ok()?,
            f_lo_hz: f[2].parse().ok()?,
            f_hi_hz: f[3].parse().ok()?,
            flags: FloorFlags::from_bits_truncate(f[4].parse().ok()?),
            gain: GainSetting {
                lna_db: f[5].parse().ok()?,
                vga_db: f[6].parse().ok()?,
                amp_on: match f[7] {
                    "0" => false,
                    "1" => true,
                    _ => return None,
                },
            },
            calibration: match f[8] {
                "-" => None,
                s => Some(s.parse().ok()?),
            },
            cal_uncertainty_db: f[9].parse().ok()?,
        })
    }
}

/// The run log (see the [module docs](self)).
#[derive(Debug)]
pub(crate) struct RunLog {
    path: PathBuf,
    file: Option<File>,
    closed: Vec<FloorRun>,
    open: Option<FloorRun>,
    pending: Option<(i64, FloorRun)>,
    cell_ns: i64,
    max_run_ns: i64,
    gap_factor: f64,
    pub corrupt_lines: u64,
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

impl RunLog {
    pub fn open(
        path: PathBuf,
        cell_ns: i64,
        max_run_ns: i64,
        gap_factor: f64,
    ) -> Result<Self, StoreError> {
        let mut closed = Vec::new();
        let mut corrupt = 0;
        match File::open(&path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let line = line.map_err(io_err(&path))?;
                    if line.starts_with('#') || line.trim().is_empty() {
                        continue;
                    }
                    match FloorRun::parse(&line) {
                        Some(r) => closed.push(r),
                        None => corrupt += 1,
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(StoreError::Io { path, source: e }),
        }
        closed.sort_by_key(|r| r.t0_ns);
        Ok(Self {
            path,
            file: None,
            closed,
            open: None,
            pending: None,
            cell_ns: cell_ns.max(1),
            max_run_ns: max_run_ns.max(1),
            gap_factor,
            corrupt_lines: corrupt,
        })
    }

    fn write(&mut self, run: FloorRun) -> Result<(), StoreError> {
        if self.file.is_none() {
            let new = !self.path.exists();
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .map_err(io_err(&self.path))?;
            if new {
                writeln!(f, "{HEADER}").map_err(io_err(&self.path))?;
            }
            self.file = Some(f);
        }
        let f = self.file.as_mut().expect("opened");
        f.write_all(run.to_line().as_bytes())
            .map_err(io_err(&self.path))?;
        let at = self.closed.partition_point(|r| r.t0_ns <= run.t0_ns);
        self.closed.insert(at, run);
        Ok(())
    }

    /// Moves the pending cell record into the open run (merging or closing it).
    fn settle(&mut self) -> Result<(), StoreError> {
        let Some((_, cell)) = self.pending.take() else {
            return Ok(());
        };
        if let Some(open) = self.open.as_mut() {
            let merge = open.same_state(&cell)
                && open.flags == cell.flags
                && cell.t1_ns - open.t0_ns <= self.max_run_ns
                && (cell.t0_ns - open.t1_ns) as f64
                    <= (self.gap_factor - 1.0).max(0.0) * (cell.t1_ns - cell.t0_ns).max(1) as f64;
            if merge {
                open.t1_ns = open.t1_ns.max(cell.t1_ns);
                return Ok(());
            }
            let done = *open;
            self.write(done)?;
        }
        self.open = Some(cell);
        Ok(())
    }

    /// Records one frame's span.
    pub fn record(&mut self, frame: FloorRun) -> Result<(), StoreError> {
        let cell = frame.t0_ns.div_euclid(self.cell_ns);
        if let Some((c, p)) = self.pending.as_mut()
            && *c == cell
            && p.same_state(&frame)
            && self_contiguous(p, &frame, self.gap_factor)
        {
            p.flags |= frame.flags;
            p.t1_ns = p.t1_ns.max(frame.t1_ns);
            return Ok(());
        }
        self.settle()?;
        self.pending = Some((cell, frame));
        Ok(())
    }

    /// Closes and writes everything in progress.
    pub fn flush(&mut self) -> Result<(), StoreError> {
        self.settle()?;
        if let Some(open) = self.open.take() {
            self.write(open)?;
        }
        if let Some(f) = self.file.as_mut() {
            f.flush().map_err(io_err(&self.path))?;
        }
        Ok(())
    }

    /// Runs (closed and in progress) overlapping `[t0, t1)` × `freq`.
    pub fn overlapping(&self, t0_ns: i64, t1_ns: i64, freq: FreqRange) -> Vec<FloorRun> {
        let from = t0_ns.saturating_sub(self.max_run_ns + self.cell_ns);
        let start = self.closed.partition_point(|r| r.t0_ns < from);
        let mut out: Vec<FloorRun> = self.closed[start..]
            .iter()
            .take_while(|r| r.t0_ns < t1_ns)
            .filter(|r| r.overlaps(t0_ns, t1_ns, &freq))
            .copied()
            .collect();
        out.extend(
            self.open
                .iter()
                .chain(self.pending.iter().map(|(_, p)| p))
                .filter(|r| r.overlaps(t0_ns, t1_ns, &freq)),
        );
        out
    }
}

fn self_contiguous(a: &FloorRun, b: &FloorRun, gap_factor: f64) -> bool {
    let dur = (b.t1_ns - b.t0_ns).max(1) as f64;
    (b.t0_ns - a.t1_ns) as f64 <= (gap_factor - 1.0).max(0.0) * dur
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: GainSetting = GainSetting {
        lna_db: 24.0,
        vga_db: 20.0,
        amp_on: false,
    };

    fn frame(t0_ms: i64, flags: FloorFlags, gain: GainSetting) -> FloorRun {
        FloorRun {
            t0_ns: t0_ms * 1_000_000,
            t1_ns: (t0_ms + 100) * 1_000_000,
            f_lo_hz: 99.5e6,
            f_hi_hz: 100.5e6,
            flags,
            gain,
            calibration: None,
            cal_uncertainty_db: 0.0,
        }
    }

    #[test]
    fn cells_or_flags_runs_merge_and_persist() {
        let dir = std::env::temp_dir().join(format!("hk-store-runs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("runs.tsv");
        let s = 1_000_000_000;
        let mut log = RunLog::open(path.clone(), s, 60 * s, 1.5).unwrap();
        // 0–3 s quiet, one impulsive frame at 3.2 s, quiet to 5 s, gain change at 5.5 s.
        for ms in (0..5000).step_by(100) {
            let f = if ms == 3200 {
                FloorFlags::IMPULSIVE_PAUSED
            } else {
                FloorFlags::NONE
            };
            log.record(frame(ms, f, G)).unwrap();
        }
        let g2 = GainSetting { lna_db: 32.0, ..G };
        for ms in (5000..5500).step_by(100) {
            log.record(frame(ms, FloorFlags::NONE, G)).unwrap();
        }
        for ms in (5500..7000).step_by(100) {
            log.record(frame(ms, FloorFlags::NONE, g2)).unwrap();
        }
        log.flush().unwrap();
        let runs = log.overlapping(0, 10 * s, FreqRange::new(0.0, 1e9));
        let spans: Vec<(i64, i64, bool)> = runs
            .iter()
            .map(|r| {
                (
                    r.t0_ns / 100_000_000,
                    r.t1_ns / 100_000_000,
                    r.flags.is_empty(),
                )
            })
            .collect();
        // [0,3) quiet, [3,4) impulsive cell, [4,5.5) quiet G, [5.5,7) G2.
        assert_eq!(
            spans,
            vec![
                (0, 30, true),
                (30, 40, false),
                (40, 55, true),
                (55, 70, true)
            ]
        );
        // Step 5–6 s sees two gain states.
        let mixed = log.overlapping(5 * s, 6 * s, FreqRange::new(0.0, 1e9));
        assert_eq!(mixed.len(), 2);
        drop(log);
        let reopened = RunLog::open(path, s, 60 * s, 1.5).unwrap();
        assert_eq!(
            reopened.overlapping(0, 10 * s, FreqRange::new(0.0, 1e9)),
            runs
        );
        assert_eq!(reopened.corrupt_lines, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
