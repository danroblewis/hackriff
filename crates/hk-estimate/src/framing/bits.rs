//! Bit helpers: packing with a bit order, hex and bit-string rendering, Hamming distance.
//!
//! Bits are `u8` values 0/1 in **transmission order** throughout the framing module.

use serde::{Deserialize, Serialize};

/// Order in which a byte's bits are transmitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BitOrder {
    /// Most significant bit first (CC1101, SX12xx, most sub-GHz transceivers).
    MsbFirst,
    /// Least significant bit first (IEEE 802.15.4, UARTs).
    LsbFirst,
}

impl BitOrder {
    /// Both orders, MSB first first (the tie-break preference).
    pub const ALL: [BitOrder; 2] = [BitOrder::MsbFirst, BitOrder::LsbFirst];
}

/// Packs whole bytes from `bits` (a trailing partial byte is dropped).
pub fn pack(bits: &[u8], order: BitOrder) -> Vec<u8> {
    bits.chunks_exact(8)
        .map(|c| {
            c.iter().enumerate().fold(0u8, |acc, (i, &b)| {
                let shift = match order {
                    BitOrder::MsbFirst => 7 - i,
                    BitOrder::LsbFirst => i,
                };
                acc | ((b & 1) << shift)
            })
        })
        .collect()
}

/// Unpacks bytes into transmission-order bits.
pub fn unpack(bytes: &[u8], order: BitOrder) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() * 8);
    for &byte in bytes {
        for i in 0..8 {
            let shift = match order {
                BitOrder::MsbFirst => 7 - i,
                BitOrder::LsbFirst => i,
            };
            out.push((byte >> shift) & 1);
        }
    }
    out
}

/// `"0101…"`.
pub fn bit_string(bits: &[u8]) -> String {
    bits.iter()
        .map(|&b| if b & 1 == 1 { '1' } else { '0' })
        .collect()
}

/// Parses `"0101…"`; any other character is an error.
pub fn parse_bit_string(s: &str) -> Option<Vec<u8>> {
    s.chars()
        .map(|c| match c {
            '0' => Some(0),
            '1' => Some(1),
            _ => None,
        })
        .collect()
}

/// Uppercase hex of whole bytes packed with `order`; `None` unless `bits.len()` is a multiple
/// of 8.
pub fn hex(bits: &[u8], order: BitOrder) -> Option<String> {
    if bits.is_empty() || bits.len() % 8 != 0 {
        return None;
    }
    Some(
        pack(bits, order)
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect(),
    )
}

/// Number of differing positions over the common length.
pub fn hamming(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).filter(|(x, y)| (*x ^ *y) & 1 == 1).count()
}

/// Bitwise complement.
pub fn inverted(bits: &[u8]) -> Vec<u8> {
    bits.iter().map(|b| 1 - (b & 1)).collect()
}

/// Binary entropy of a share `p` of ones, bits.
pub(crate) fn binary_entropy(p: f64) -> f64 {
    if p <= 0.0 || p >= 1.0 {
        0.0
    } else {
        -(p * p.log2() + (1.0 - p) * (1.0 - p).log2())
    }
}

/// Shannon entropy of a byte histogram, bits per byte.
pub(crate) fn byte_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut h = [0usize; 256];
    for &b in bytes {
        h[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    h.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trip_both_orders() {
        let bytes = [0x2D, 0xD4, 0x01, 0x80];
        for order in BitOrder::ALL {
            let bits = unpack(&bytes, order);
            assert_eq!(pack(&bits, order), bytes);
        }
        assert_eq!(bit_string(&unpack(&[0x2D], BitOrder::MsbFirst)), "00101101");
        assert_eq!(bit_string(&unpack(&[0x2D], BitOrder::LsbFirst)), "10110100");
        assert_eq!(
            hex(
                &unpack(&[0x0C, 0x5F], BitOrder::MsbFirst),
                BitOrder::MsbFirst
            )
            .as_deref(),
            Some("0C5F")
        );
        assert_eq!(parse_bit_string("0110"), Some(vec![0, 1, 1, 0]));
        assert_eq!(parse_bit_string("01x"), None);
    }
}
