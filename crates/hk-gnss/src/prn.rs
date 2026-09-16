//! GPS L1 C/A Gold codes — the *known codes* the whole C36 exception exists for.
//!
//! A C/A code is the modulo-2 sum of two 10-stage maximal-length shift registers (IS-GPS-200):
//!
//! - `G1`: feedback polynomial `1 + x³ + x¹⁰`.
//! - `G2`: feedback polynomial `1 + x² + x³ + x⁶ + x⁸ + x⁹ + x¹⁰`.
//!
//! Both start at all-ones. Each satellite takes a different *phase* of `G2`, selected by a pair
//! of taps ([`TAPS`]) rather than by delaying the register — the two are equivalent and the tap
//! form is what the standard tabulates.
//!
//! The result is a 1023-chip Gold sequence at 1.023 Mchip/s, so the code repeats every 1 ms.
//! Its correlation properties are the reason acquisition works at all at 20–30 dB below the
//! noise floor: correlating against the right code for 1 ms buys ~30 dB of processing gain
//! (`10·log₁₀(1023)`), which is what lifts a below-noise signal into view.
//!
//! These codes are **public constants**, not measurements. Nothing here observes anything.

use std::fmt;

/// Chips in one C/A code period.
pub const CODE_LENGTH: usize = 1023;

/// C/A chipping rate, chips per second.
pub const CHIP_RATE_HZ: f64 = 1_023_000.0;

/// One C/A code period, seconds.
pub const CODE_PERIOD_S: f64 = 1.0e-3;

/// GPS L1 centre frequency.
pub const L1_HZ: f64 = 1_575_420_000.0;

/// GPS L5 centre frequency (recorded for completeness; no L5 codes are generated here).
pub const L5_HZ: f64 = 1_176_450_000.0;

/// `G2` phase-selector tap pairs for PRN 1..=32 (IS-GPS-200 Table 3-I), as 1-based stage numbers.
/// The satellite's code is `G1 ⊕ (G2[a] ⊕ G2[b])`.
const TAPS: [(u8, u8); 32] = [
    (2, 6),
    (3, 7),
    (4, 8),
    (5, 9),
    (1, 9),
    (2, 10),
    (1, 8),
    (2, 9),
    (3, 10),
    (2, 3),
    (3, 4),
    (5, 6),
    (6, 7),
    (7, 8),
    (8, 9),
    (9, 10),
    (1, 4),
    (2, 5),
    (3, 6),
    (4, 7),
    (5, 8),
    (6, 9),
    (1, 3),
    (4, 6),
    (5, 7),
    (6, 8),
    (7, 9),
    (8, 10),
    (1, 6),
    (2, 7),
    (3, 8),
    (4, 9),
];

/// The highest PRN this codebook generates.
pub const MAX_PRN: u8 = 32;

/// Why a PRN could not be produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PrnError {
    /// PRN outside 1..=32.
    #[error("PRN {0} is out of range 1..={MAX_PRN}")]
    OutOfRange(u8),
}

/// One satellite's 1023-chip C/A code, stored as ±1 (chip `0` → `+1`, chip `1` → `−1`).
#[derive(Clone)]
pub struct CaCode {
    prn: u8,
    chips: Box<[i8; CODE_LENGTH]>,
}

impl fmt::Debug for CaCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaCode")
            .field("prn", &self.prn)
            .field("len", &CODE_LENGTH)
            .finish()
    }
}

impl CaCode {
    /// Generates the C/A code for `prn` (1..=32).
    pub fn new(prn: u8) -> Result<Self, PrnError> {
        if prn == 0 || prn > MAX_PRN {
            return Err(PrnError::OutOfRange(prn));
        }
        let (tap_a, tap_b) = TAPS[usize::from(prn) - 1];
        // Stage 1 is index 0, stage 10 is index 9. Output is taken from stage 10; feedback is
        // shifted into stage 1.
        let mut g1 = [1u8; 10];
        let mut g2 = [1u8; 10];
        let mut chips = Box::new([0i8; CODE_LENGTH]);

        for chip in chips.iter_mut() {
            let g2_phase = g2[usize::from(tap_a) - 1] ^ g2[usize::from(tap_b) - 1];
            let bit = g1[9] ^ g2_phase;
            // 0 → +1, 1 → −1 (BPSK). A consistent global mapping leaves the correlation
            // magnitudes unchanged.
            *chip = 1 - 2 * (bit as i8);

            let g1_fb = g1[2] ^ g1[9];
            let g2_fb = g2[1] ^ g2[2] ^ g2[5] ^ g2[7] ^ g2[8] ^ g2[9];
            g1.copy_within(0..9, 1);
            g1[0] = g1_fb;
            g2.copy_within(0..9, 1);
            g2[0] = g2_fb;
        }

        Ok(Self { prn, chips })
    }

    /// The satellite this code belongs to.
    pub fn prn(&self) -> u8 {
        self.prn
    }

    /// The 1023 chips as ±1.
    pub fn chips(&self) -> &[i8; CODE_LENGTH] {
        &self.chips
    }

    /// The first ten chips packed MSB-first into a 10-bit word, as `1` bits for `−1` chips.
    /// IS-GPS-200 tabulates this as an octal value, so it is the cheapest way to check a
    /// generator against the standard.
    pub fn first_ten_chips_octal(&self) -> u16 {
        let mut word = 0u16;
        for &chip in self.chips[..10].iter() {
            word = (word << 1) | u16::from(chip < 0);
        }
        word
    }

    /// Periodic correlation of this code against `other` at `lag` chips, in chips
    /// (i.e. ±1 units summed over the period).
    pub fn correlate(&self, other: &CaCode, lag: usize) -> i32 {
        self.chips
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let b = other.chips[(i + lag) % CODE_LENGTH];
                i32::from(a) * i32::from(b)
            })
            .sum()
    }
}

/// The set of known PRN codes acquisition despreads against.
///
/// Holding one of these is what it means to be on the known-signal-led path: there is no way to
/// acquire GPS L1 without the published codes, which is exactly why C36 is an exception to
/// blind-first. See the crate docs.
#[derive(Clone, Debug)]
pub struct PrnCodebook {
    name: &'static str,
    codes: Vec<CaCode>,
}

impl PrnCodebook {
    /// The 32 GPS L1 C/A codes.
    pub fn gps_l1_ca() -> Self {
        let codes = (1..=MAX_PRN)
            .map(|prn| CaCode::new(prn).expect("PRN 1..=32 is in range"))
            .collect();
        Self {
            name: "gps-l1-ca@is-gps-200",
            codes,
        }
    }

    /// A codebook holding only the listed PRNs, for a narrowed search.
    pub fn subset(prns: &[u8]) -> Result<Self, PrnError> {
        let codes = prns
            .iter()
            .map(|&prn| CaCode::new(prn))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            name: "gps-l1-ca@is-gps-200",
            codes,
        })
    }

    /// A stable identifier for the code set, recorded on every acquisition as provenance.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The codes in the book.
    pub fn codes(&self) -> &[CaCode] {
        &self.codes
    }

    /// The code for `prn`, if this book carries it.
    pub fn get(&self, prn: u8) -> Option<&CaCode> {
        self.codes.iter().find(|c| c.prn == prn)
    }

    /// How many satellites this book can search for.
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Whether the book is empty.
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Gold code from a preferred pair of m-sequences is balanced: 512 `−1` chips against
    /// 511 `+1`, so the chips sum to exactly −1.
    #[test]
    fn every_code_is_balanced() {
        for prn in 1..=MAX_PRN {
            let code = CaCode::new(prn).unwrap();
            let sum: i32 = code.chips().iter().map(|&c| i32::from(c)).sum();
            assert_eq!(sum, -1, "PRN {prn} is not balanced");
        }
    }

    /// The defining property of a Gold code of period 1023 (m = 10, even): every off-peak
    /// autocorrelation and every cross-correlation takes one of exactly three values,
    /// {−1, −65, 63}, against a peak of 1023. This is what makes despreading work, and it is a
    /// far stronger check on the generator than any single tabulated constant.
    #[test]
    fn correlation_is_three_valued() {
        const ALLOWED: [i32; 3] = [-1, -65, 63];
        let codes: Vec<CaCode> = (1..=8).map(|p| CaCode::new(p).unwrap()).collect();

        for code in &codes {
            assert_eq!(code.correlate(code, 0), CODE_LENGTH as i32);
            for lag in 1..CODE_LENGTH {
                let r = code.correlate(code, lag);
                assert!(
                    ALLOWED.contains(&r),
                    "PRN {} autocorrelation at lag {lag} was {r}",
                    code.prn()
                );
            }
        }

        for a in &codes {
            for b in &codes {
                if a.prn() == b.prn() {
                    continue;
                }
                for lag in 0..CODE_LENGTH {
                    let r = a.correlate(b, lag);
                    assert!(
                        ALLOWED.contains(&r),
                        "PRN {} × PRN {} at lag {lag} was {r}",
                        a.prn(),
                        b.prn()
                    );
                }
            }
        }
    }

    #[test]
    fn prn_range_is_checked() {
        assert_eq!(CaCode::new(0).unwrap_err(), PrnError::OutOfRange(0));
        assert_eq!(CaCode::new(33).unwrap_err(), PrnError::OutOfRange(33));
        assert!(CaCode::new(1).is_ok());
        assert!(CaCode::new(32).is_ok());
    }

    #[test]
    fn codebook_carries_every_satellite() {
        let book = PrnCodebook::gps_l1_ca();
        assert_eq!(book.len(), usize::from(MAX_PRN));
        for prn in 1..=MAX_PRN {
            assert_eq!(book.get(prn).unwrap().prn(), prn);
        }
        assert!(book.get(33).is_none());
    }
}
