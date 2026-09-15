//! CRC engine and catalogue (CRC RevEng parameter model: width, poly, init, refin, refout,
//! xorout; check = CRC of ASCII `"123456789"`).
//!
//! The catalogue is the common CRC-8/16/32 set seen in sub-GHz ISM, 802.15.4, wM-Bus, DNP3 and
//! file/network formats. The search ([`super::search`]) also tries every catalogue polynomial
//! with and without reflection and with init/xorout ∈ {0, all ones}; a variant that matches a
//! catalogue entry takes its name.

use serde::{Deserialize, Serialize};

/// A CRC in the RevEng parameter model. `poly` and `init` are in normal (unreflected) form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CrcParams {
    /// Width, bits (8..=32).
    pub width: u8,
    /// Generator polynomial, normal form, without the x^width term.
    pub poly: u32,
    /// Register initial value, normal form.
    pub init: u32,
    /// Input bytes reflected.
    pub refin: bool,
    /// Output reflected.
    pub refout: bool,
    /// Final XOR.
    pub xorout: u32,
}

impl CrcParams {
    /// All-ones mask for the width.
    pub fn mask(&self) -> u32 {
        width_mask(self.width)
    }

    /// CRC of `data`. Only `refin == refout` (every catalogue entry) is supported; a mixed
    /// setting is computed as `refin`.
    pub fn compute(&self, data: &[u8]) -> u32 {
        let core = CrcCore::new(self.width, self.poly, self.refin);
        core.run(core.internal_init(self.init), data) ^ (self.xorout & self.mask())
    }

    /// The catalogue entry with exactly these parameters, if any.
    pub fn catalogue_entry(&self) -> Option<&'static CatalogueEntry> {
        CATALOGUE.iter().find(|e| e.params == *self)
    }

    /// Catalogue name, or a RevEng-style description of the variant.
    pub fn name(&self) -> String {
        match self.catalogue_entry() {
            Some(e) => e.name.to_owned(),
            None => format!(
                "CRC-{}/variant(poly=0x{:0w$X},init=0x{:0w$X},refin={},refout={},xorout=0x{:0w$X})",
                self.width,
                self.poly,
                self.init,
                self.refin,
                self.refout,
                self.xorout,
                w = usize::from(self.width).div_ceil(4)
            ),
        }
    }
}

/// A named catalogue CRC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogueEntry {
    /// RevEng name.
    pub name: &'static str,
    /// Other common names.
    pub aliases: &'static [&'static str],
    /// Parameters.
    pub params: CrcParams,
    /// CRC of ASCII `"123456789"`.
    pub check: u32,
}

const fn p(width: u8, poly: u32, init: u32, refl: bool, xorout: u32) -> CrcParams {
    CrcParams {
        width,
        poly,
        init,
        refin: refl,
        refout: refl,
        xorout,
    }
}

/// The catalogue, in tie-break preference order (earlier wins when two descriptions validate
/// equally, e.g. CRC-16/CCITT-FALSE MSB-first vs CRC-16/MCRF4XX over bit-reversed bytes).
pub const CATALOGUE: &[CatalogueEntry] = &[
    CatalogueEntry {
        name: "CRC-16/CCITT-FALSE",
        aliases: &["CRC-16/IBM-3740", "CRC-16/AUTOSAR"],
        params: p(16, 0x1021, 0xFFFF, false, 0),
        check: 0x29B1,
    },
    CatalogueEntry {
        name: "CRC-16/KERMIT",
        aliases: &[
            "CRC-16/CCITT",
            "CRC-16/IEEE-802.15.4 (2-octet FCS)",
            "CRC-CCITT-TRUE",
        ],
        params: p(16, 0x1021, 0x0000, true, 0),
        check: 0x2189,
    },
    CatalogueEntry {
        name: "CRC-16/XMODEM",
        aliases: &["CRC-16/ACORN", "CRC-16/LTE", "CRC-16/V-41-MSB", "ZMODEM"],
        params: p(16, 0x1021, 0x0000, false, 0),
        check: 0x31C3,
    },
    CatalogueEntry {
        name: "CRC-16/ARC",
        aliases: &["CRC-16/IBM", "CRC-16/LHA", "CRC-IBM"],
        params: p(16, 0x8005, 0x0000, true, 0),
        check: 0xBB3D,
    },
    CatalogueEntry {
        name: "CRC-16/DNP",
        aliases: &[],
        params: p(16, 0x3D65, 0x0000, true, 0xFFFF),
        check: 0xEA82,
    },
    CatalogueEntry {
        name: "CRC-32/ISO-HDLC",
        aliases: &[
            "CRC-32",
            "CRC-32/ADCCP",
            "PKZIP",
            "IEEE 802.15.4g (4-octet FCS)",
        ],
        params: p(32, 0x04C1_1DB7, 0xFFFF_FFFF, true, 0xFFFF_FFFF),
        check: 0xCBF4_3926,
    },
    CatalogueEntry {
        name: "CRC-16/CMS",
        aliases: &["CC1101/CC2500 hardware CRC"],
        params: p(16, 0x8005, 0xFFFF, false, 0),
        check: 0xAEE7,
    },
    CatalogueEntry {
        name: "CRC-16/EN-13757",
        aliases: &["wM-Bus"],
        params: p(16, 0x3D65, 0x0000, false, 0xFFFF),
        check: 0xC2B7,
    },
    CatalogueEntry {
        name: "CRC-16/IBM-SDLC",
        aliases: &["CRC-16/X-25", "CRC-16/ISO-HDLC", "CRC-B"],
        params: p(16, 0x1021, 0xFFFF, true, 0xFFFF),
        check: 0x906E,
    },
    CatalogueEntry {
        name: "CRC-16/GENIBUS",
        aliases: &["CRC-16/DARC", "CRC-16/EPC", "CRC-16/I-CODE"],
        params: p(16, 0x1021, 0xFFFF, false, 0xFFFF),
        check: 0xD64E,
    },
    CatalogueEntry {
        name: "CRC-16/MCRF4XX",
        aliases: &[],
        params: p(16, 0x1021, 0xFFFF, true, 0),
        check: 0x6F91,
    },
    CatalogueEntry {
        name: "CRC-16/SPI-FUJITSU",
        aliases: &["CRC-16/AUG-CCITT"],
        params: p(16, 0x1021, 0x1D0F, false, 0),
        check: 0xE5CC,
    },
    CatalogueEntry {
        name: "CRC-16/MODBUS",
        aliases: &[],
        params: p(16, 0x8005, 0xFFFF, true, 0),
        check: 0x4B37,
    },
    CatalogueEntry {
        name: "CRC-16/UMTS",
        aliases: &["CRC-16/BUYPASS", "CRC-16/VERIFONE"],
        params: p(16, 0x8005, 0x0000, false, 0),
        check: 0xFEE8,
    },
    CatalogueEntry {
        name: "CRC-16/MAXIM-DOW",
        aliases: &["CRC-16/MAXIM"],
        params: p(16, 0x8005, 0x0000, true, 0xFFFF),
        check: 0x44C2,
    },
    CatalogueEntry {
        name: "CRC-16/USB",
        aliases: &[],
        params: p(16, 0x8005, 0xFFFF, true, 0xFFFF),
        check: 0xB4C8,
    },
    CatalogueEntry {
        name: "CRC-32/BZIP2",
        aliases: &["CRC-32/AAL5", "CRC-32/DECT-B"],
        params: p(32, 0x04C1_1DB7, 0xFFFF_FFFF, false, 0xFFFF_FFFF),
        check: 0xFC89_1918,
    },
    CatalogueEntry {
        name: "CRC-32/MPEG-2",
        aliases: &[],
        params: p(32, 0x04C1_1DB7, 0xFFFF_FFFF, false, 0),
        check: 0x0376_E6E7,
    },
    CatalogueEntry {
        name: "CRC-32/ISCSI",
        aliases: &["CRC-32C", "CRC-32/CASTAGNOLI"],
        params: p(32, 0x1EDC_6F41, 0xFFFF_FFFF, true, 0xFFFF_FFFF),
        check: 0xE306_9283,
    },
    CatalogueEntry {
        name: "CRC-32/JAMCRC",
        aliases: &[],
        params: p(32, 0x04C1_1DB7, 0xFFFF_FFFF, true, 0),
        check: 0x340B_C6D9,
    },
    CatalogueEntry {
        name: "CRC-32/CKSUM",
        aliases: &["CRC-32/POSIX"],
        params: p(32, 0x04C1_1DB7, 0x0000_0000, false, 0xFFFF_FFFF),
        check: 0x765E_7680,
    },
    CatalogueEntry {
        name: "CRC-8/SMBUS",
        aliases: &["CRC-8"],
        params: p(8, 0x07, 0x00, false, 0),
        check: 0xF4,
    },
    CatalogueEntry {
        name: "CRC-8/MAXIM-DOW",
        aliases: &["CRC-8/MAXIM", "DOW-CRC"],
        params: p(8, 0x31, 0x00, true, 0),
        check: 0xA1,
    },
    CatalogueEntry {
        name: "CRC-8/I-432-1",
        aliases: &["CRC-8/ITU"],
        params: p(8, 0x07, 0x00, false, 0x55),
        check: 0xA1,
    },
    CatalogueEntry {
        name: "CRC-8/ROHC",
        aliases: &[],
        params: p(8, 0x07, 0xFF, true, 0),
        check: 0xD0,
    },
    CatalogueEntry {
        name: "CRC-8/CDMA2000",
        aliases: &[],
        params: p(8, 0x9B, 0xFF, false, 0),
        check: 0xDA,
    },
    CatalogueEntry {
        name: "CRC-8/DVB-S2",
        aliases: &[],
        params: p(8, 0xD5, 0x00, false, 0),
        check: 0xBC,
    },
    CatalogueEntry {
        name: "CRC-8/AUTOSAR",
        aliases: &[],
        params: p(8, 0x2F, 0xFF, false, 0xFF),
        check: 0xDF,
    },
    CatalogueEntry {
        name: "CRC-8/SAE-J1850",
        aliases: &[],
        params: p(8, 0x1D, 0xFF, false, 0xFF),
        check: 0x4B,
    },
];

pub(crate) fn width_mask(width: u8) -> u32 {
    if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    }
}

pub(crate) fn reflect(v: u32, width: u8) -> u32 {
    let mut r = 0u32;
    for i in 0..width {
        if v & (1 << i) != 0 {
            r |= 1 << (width - 1 - i);
        }
    }
    r
}

/// The linear core of a CRC: width, polynomial and reflection, table-driven. The register it
/// carries is the *internal* one (reflected for reflected CRCs), and its final value is the CRC
/// before `xorout`.
#[derive(Clone, Debug)]
pub(crate) struct CrcCore {
    pub width: u8,
    pub poly: u32,
    pub refl: bool,
    table: [u32; 256],
}

impl CrcCore {
    pub fn new(width: u8, poly: u32, refl: bool) -> Self {
        assert!(
            (8..=32).contains(&width),
            "CRC width {width} outside 8..=32"
        );
        let mask = width_mask(width);
        let mut table = [0u32; 256];
        if refl {
            let rpoly = reflect(poly & mask, width);
            for (i, t) in table.iter_mut().enumerate() {
                let mut r = i as u32;
                for _ in 0..8 {
                    r = if r & 1 == 1 { (r >> 1) ^ rpoly } else { r >> 1 };
                }
                *t = r & mask;
            }
        } else {
            let top = 1u64 << (width - 1);
            for (i, t) in table.iter_mut().enumerate() {
                let mut r = (i as u64) << (width - 8);
                for _ in 0..8 {
                    r = if r & top != 0 {
                        (r << 1) ^ u64::from(poly)
                    } else {
                        r << 1
                    };
                }
                *t = (r as u32) & mask;
            }
        }
        Self {
            width,
            poly: poly & mask,
            refl,
            table,
        }
    }

    pub fn mask(&self) -> u32 {
        width_mask(self.width)
    }

    /// The internal register for a normal-form `init`.
    pub fn internal_init(&self, init: u32) -> u32 {
        if self.refl {
            reflect(init & self.mask(), self.width)
        } else {
            init & self.mask()
        }
    }

    #[inline]
    pub fn update(&self, reg: u32, byte: u8) -> u32 {
        if self.refl {
            (reg >> 8) ^ self.table[((reg ^ u32::from(byte)) & 0xFF) as usize]
        } else {
            let idx = ((reg >> (self.width - 8)) ^ u32::from(byte)) & 0xFF;
            (((u64::from(reg) << 8) as u32) ^ self.table[idx as usize]) & self.mask()
        }
    }

    pub fn run(&self, reg: u32, data: &[u8]) -> u32 {
        data.iter().fold(reg, |r, &b| self.update(r, b))
    }

    /// The register after `n` zero bytes from `reg` (the init contribution over `n` bytes).
    pub fn run_zeros(&self, mut reg: u32, n: usize) -> u32 {
        for _ in 0..n {
            reg = self.update(reg, 0);
        }
        reg
    }

    /// Parameters for this core with `init` / `xorout`.
    pub fn params(&self, init: u32, xorout: u32) -> CrcParams {
        CrcParams {
            width: self.width,
            poly: self.poly,
            init: init & self.mask(),
            refin: self.refl,
            refout: self.refl,
            xorout: xorout & self.mask(),
        }
    }
}

/// A CRC in the RevEng model over a **bit range** of MSB-first packed bytes, for any width
/// 1..=32 and any bit count (the `crc` block, T-087; RDS's 10-bit block check, CRC-24 over an
/// 88-bit ADS-B body). The register is the normal MSB-first one; `refin` feeds each 8-bit
/// group of the range (from its first bit; a trailing partial group likewise) last bit first,
/// `refout` reverses the register before `xorout`. Whole-byte ranges with width ≥ 8 and
/// `refin == refout` run on the table-driven [`CrcCore`]; everything else bit-serially.
#[derive(Clone, Debug)]
pub struct BitCrc {
    width: u8,
    poly: u32,
    init: u32,
    refin: bool,
    refout: bool,
    xorout: u32,
    core: Option<CrcCore>,
}

impl BitCrc {
    /// A CRC; `poly` in normal form or full form (with the x^width term, e.g. RDS `0x5B9`), the
    /// top term implied either way. `None` for a width outside 1..=32 or a wider polynomial.
    pub fn new(
        width: u8,
        poly: u64,
        init: u32,
        refin: bool,
        refout: bool,
        xorout: u32,
    ) -> Option<Self> {
        if !(1..=32).contains(&width) || poly >> (u32::from(width) + 1) != 0 {
            return None;
        }
        let mask = width_mask(width);
        let poly = (poly & u64::from(mask)) as u32;
        let core = (width >= 8 && refin == refout).then(|| CrcCore::new(width, poly, refin));
        Some(Self {
            width,
            poly,
            init: init & mask,
            refin,
            refout,
            xorout: xorout & mask,
            core,
        })
    }

    /// Width, bits.
    pub fn width(&self) -> u8 {
        self.width
    }

    /// All-ones mask for the width.
    pub fn mask(&self) -> u32 {
        width_mask(self.width)
    }

    /// CRC of `n_bits` bits of `bytes` from bit `start_bit` (bit 0 = MSB of byte 0). The range
    /// must lie inside `bytes`.
    pub fn compute(&self, bytes: &[u8], start_bit: usize, n_bits: usize) -> u32 {
        self.run(bytes, start_bit, n_bits, self.init, self.xorout)
    }

    /// The linear part (init and xorout 0): `compute(d ^ e) = compute(d) ^ linear(e)` for
    /// ranges of equal length, which syndrome-based correction relies on.
    pub fn linear(&self, bytes: &[u8], start_bit: usize, n_bits: usize) -> u32 {
        self.run(bytes, start_bit, n_bits, 0, 0)
    }

    fn run(&self, bytes: &[u8], start_bit: usize, n_bits: usize, init: u32, xorout: u32) -> u32 {
        if let Some(core) = &self.core
            && start_bit % 8 == 0
            && n_bits % 8 == 0
        {
            let s = start_bit / 8;
            return core.run(core.internal_init(init), &bytes[s..s + n_bits / 8]) ^ xorout;
        }
        self.serial(bytes, start_bit, n_bits, init) ^ xorout
    }

    fn serial(&self, bytes: &[u8], start_bit: usize, n_bits: usize, init: u32) -> u32 {
        let w = u32::from(self.width);
        let mask = u64::from(self.mask());
        let top = 1u64 << (w - 1);
        let poly = u64::from(self.poly);
        let bit = |i: usize| u64::from((bytes[i / 8] >> (7 - i % 8)) & 1);
        let mut reg = u64::from(init);
        let mut feed = |b: u64| {
            let fb = u64::from(reg & top != 0) ^ b;
            reg = (reg << 1) & mask;
            if fb == 1 {
                reg ^= poly;
            }
        };
        let end = start_bit + n_bits;
        if self.refin {
            let mut g = start_bit;
            while g < end {
                let ge = (g + 8).min(end);
                for i in (g..ge).rev() {
                    feed(bit(i));
                }
                g = ge;
            }
        } else {
            for i in start_bit..end {
                feed(bit(i));
            }
        }
        let reg = reg as u32;
        if self.refout {
            reflect(reg, self.width)
        } else {
            reg
        }
    }
}

/// Byte order of a transmitted CRC field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endianness {
    /// Most significant byte first.
    Big,
    /// Least significant byte first.
    Little,
}

/// Reads a `width`-bit field from `bytes` (whole bytes).
pub(crate) fn read_field(bytes: &[u8], width: u8, e: Endianness) -> u32 {
    let n = usize::from(width / 8);
    let it = bytes[..n].iter().map(|&b| u32::from(b));
    match e {
        Endianness::Big => it.fold(0, |acc, b| (acc << 8) | b),
        Endianness::Little => it.rev().fold(0, |acc, b| (acc << 8) | b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_check_values() {
        for e in CATALOGUE {
            assert_eq!(
                e.params.compute(b"123456789"),
                e.check,
                "{} check value",
                e.name
            );
            assert_eq!(e.params.name(), e.name);
        }
        // Names are unique.
        for (i, a) in CATALOGUE.iter().enumerate() {
            assert!(CATALOGUE[i + 1..].iter().all(|b| b.params != a.params));
        }
    }

    #[test]
    fn linear_decomposition_holds() {
        // crc(init, d) = crc(init, 0^n) ^ crc(0, d): what the differential search relies on.
        let data = b"\x5a\x3c\x07\x0b\x93\x21\xff\x00";
        for e in CATALOGUE {
            let core = CrcCore::new(e.params.width, e.params.poly, e.params.refin);
            let i = core.internal_init(e.params.init);
            assert_eq!(
                core.run(i, data),
                core.run_zeros(i, data.len()) ^ core.run(0, data),
                "{}",
                e.name
            );
        }
    }

    #[test]
    fn bit_crc_matches_the_catalogue_on_both_paths() {
        for e in CATALOGUE {
            let c = e.params;
            let crc = BitCrc::new(
                c.width,
                u64::from(c.poly),
                c.init,
                c.refin,
                c.refout,
                c.xorout,
            )
            .unwrap();
            assert_eq!(
                crc.compute(b"123456789", 0, 72),
                e.check,
                "{} table",
                e.name
            );
            let serial = crc.serial(b"123456789", 0, 72, c.init) ^ c.xorout;
            assert_eq!(serial, e.check, "{} serial", e.name);
            // A range that starts mid-byte takes the serial path and sees the same bits.
            let mut shifted = vec![0u8; 10];
            for (i, &b) in b"123456789".iter().enumerate() {
                shifted[i] |= b >> 3;
                shifted[i + 1] |= b << 5;
            }
            assert_eq!(crc.compute(&shifted, 3, 72), e.check, "{} offset", e.name);
        }
        // Full-form polynomial (RDS 0x5B9 = x¹⁰ + …) equals its normal form.
        let full = BitCrc::new(10, 0x5B9, 0, false, false, 0).unwrap();
        let normal = BitCrc::new(10, 0x1B9, 0, false, false, 0).unwrap();
        assert_eq!(
            full.compute(&[0xC0, 0xDE], 0, 16),
            normal.compute(&[0xC0, 0xDE], 0, 16)
        );
        assert!(BitCrc::new(10, 0xFFF, 0, false, false, 0).is_none());
        assert!(BitCrc::new(33, 0x1, 0, false, false, 0).is_none());
    }

    #[test]
    fn variant_names() {
        let v = p(16, 0x1021, 0x0000, true, 0xFFFF);
        assert!(v.name().starts_with("CRC-16/variant(poly=0x1021"));
        assert_eq!(read_field(&[0x12, 0x34], 16, Endianness::Big), 0x1234);
        assert_eq!(read_field(&[0x12, 0x34], 16, Endianness::Little), 0x3412);
    }
}
