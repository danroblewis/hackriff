//! Standard data-whitening LFSRs, used **only** to reveal frame structure (CRC validity,
//! length fields, byte statistics). Whitening is a fixed, published scrambler, not a security
//! mechanism; nothing beyond these standard sequences is ever tried (CLAUDE.md legal
//! guardrails: an encrypted-looking payload is labelled and left alone).
//!
//! Sequences are XOR masks in **transmission order**, starting at the first bit after the sync
//! word.

use serde::{Deserialize, Serialize};

/// A standard whitening sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Whitening {
    /// PN9 `x^9 + x^5 + 1`, seed `0x1FF`, bit-serial in transmission order (IEEE 802.15.4g
    /// SUN-FSK data whitening).
    Pn9Serial,
    /// PN9 `x^9 + x^5 + 1`, seed `0x1FF`, byte mode: each MSB-first byte XORed with the low byte
    /// of the register (TI CC1101/CC2500, Semtech SX12xx).
    Pn9Cc1101,
    /// `x^7 + x^4 + 1`, bit-serial with the given non-zero 7-bit seed (IEEE 802.11 scrambler;
    /// Bluetooth LE whitening with a channel-derived seed).
    Lfsr7 {
        /// Register seed, 1..=127.
        seed: u8,
    },
    /// The reciprocal `x^7 + x^3 + 1` orientation of [`Whitening::Lfsr7`] (the time-reversed
    /// m-sequence), bit-serial with the given seed.
    Lfsr7Reciprocal {
        /// Register seed, 1..=127.
        seed: u8,
    },
}

impl Whitening {
    /// PN9 variants, cheapest to try first.
    pub const PN9: [Whitening; 2] = [Whitening::Pn9Serial, Whitening::Pn9Cc1101];

    /// Every standard sequence tried: both PN9 variants, then all 127 seeds of both 7-bit
    /// orientations.
    pub fn all() -> Vec<Whitening> {
        let mut v = Self::PN9.to_vec();
        v.extend((1..=127u8).map(|seed| Whitening::Lfsr7 { seed }));
        v.extend((1..=127u8).map(|seed| Whitening::Lfsr7Reciprocal { seed }));
        v
    }

    /// Short name, e.g. `pn9-serial`, `lfsr7(seed=0x25)`.
    pub fn name(&self) -> String {
        match self {
            Whitening::Pn9Serial => "pn9-serial".into(),
            Whitening::Pn9Cc1101 => "pn9-cc1101".into(),
            Whitening::Lfsr7 { seed } => format!("lfsr7-x7x4(seed=0x{seed:02X})"),
            Whitening::Lfsr7Reciprocal { seed } => format!("lfsr7-x7x3(seed=0x{seed:02X})"),
        }
    }

    /// The first `n` mask bits, transmission order.
    pub fn sequence(&self, n: usize) -> Vec<u8> {
        match *self {
            Whitening::Pn9Serial => pn9_serial(n),
            Whitening::Pn9Cc1101 => {
                let mut serial = pn9_serial(n.div_ceil(8) * 8);
                for chunk in serial.chunks_exact_mut(8) {
                    chunk.reverse();
                }
                serial.truncate(n);
                serial
            }
            Whitening::Lfsr7 { seed } => {
                let mut s = u32::from(seed & 0x7F).max(1);
                (0..n)
                    .map(|_| {
                        let out = ((s >> 6) ^ (s >> 3)) & 1;
                        s = ((s << 1) | out) & 0x7F;
                        out as u8
                    })
                    .collect()
            }
            Whitening::Lfsr7Reciprocal { seed } => {
                let mut s = u32::from(seed & 0x7F).max(1);
                (0..n)
                    .map(|_| {
                        let out = (s ^ (s >> 3)) & 1;
                        s = (s >> 1) | (out << 6);
                        out as u8
                    })
                    .collect()
            }
        }
    }

    /// XORs the mask onto `bits` in place.
    pub fn apply(&self, bits: &mut [u8]) {
        let seq = self.sequence(bits.len());
        for (b, w) in bits.iter_mut().zip(seq) {
            *b ^= w;
        }
    }
}

fn pn9_serial(n: usize) -> Vec<u8> {
    let mut s: u32 = 0x1FF;
    (0..n)
        .map(|_| {
            let out = (s & 1) as u8;
            let fb = (s ^ (s >> 5)) & 1;
            s = (s >> 1) | (fb << 8);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::bits::{BitOrder, pack};

    #[test]
    fn pn9_cc1101_first_bytes() {
        // TI DN509: the CC1101 PN9 whitening sequence starts FF E1 1D 9A.
        let seq = Whitening::Pn9Cc1101.sequence(32);
        assert_eq!(pack(&seq, BitOrder::MsbFirst), [0xFF, 0xE1, 0x1D, 0x9A]);
        // The serial (802.15.4g) sequence is the same register, LSB of each byte first.
        let serial = Whitening::Pn9Serial.sequence(32);
        assert_eq!(pack(&serial, BitOrder::LsbFirst), [0xFF, 0xE1, 0x1D, 0x9A]);
    }

    #[test]
    fn lfsr7_is_an_m_sequence() {
        for w in [
            Whitening::Lfsr7 { seed: 0x5B },
            Whitening::Lfsr7Reciprocal { seed: 0x5B },
        ] {
            let s = w.sequence(254);
            assert_eq!(&s[..127], &s[127..], "{w:?} period 127");
            assert_eq!(s[..127].iter().filter(|&&b| b == 1).count(), 64);
        }
        assert_eq!(Whitening::all().len(), 256);
    }
}
