//! `descramble`: LFSR de-whitening (T-608, ADR-0011 §9.1), on a bit stream or per frame.
//!
//! One generic linear-feedback shift register covers the whitening/scrambling families of
//! docs/18 §6.2 (CCSDS, Meteor-M LRPT, HRIT/LRIT, HRPT, VDL2, BLE, Remote ID over BLE,
//! radiosondes, TETRA, LoRa) by parameters, not by named presets:
//!
//! - **`poly`** is the *characteristic* polynomial in full form, bit `i` = coefficient of
//!   `x^i`, the top set bit its degree `n` (the register length), and `x^0` must be present. The
//!   sequence obeys `a[k+n] = Σ c_i · a[k+i]` (mod 2): the tap at `x^i` sits `n − i` bits back.
//!   This is how CCSDS 131.0-B (`x^8+x^7+x^5+x^3+1` → `0x1A9`) and TI/IEEE 802.15.4g
//!   (`x^9+x^5+1` → `0x221`) write theirs. A standard that writes a *delay* (connection)
//!   polynomial — G3RUH/V.35 style `1 + x^12 + x^17`, taps 12 and 17 bits back — is the
//!   reciprocal: `x^17 + x^5 + 1` → `0x20021`.
//! - **`register`** `fibonacci` (default): `init` is the register, bit `j` = the `j`-th mask bit
//!   produced (bit 0 first), so CCSDS (`0xFF`) and PN9 (`0x1FF`) read as published. `galois`:
//!   the register the Bluetooth LE core spec draws (Vol 6 Part B §3.2) — shift right, output
//!   bit 0, and on a 1 fold the taps back in at bit `n−1−i` for each `x^i`; BLE's
//!   "position 0 = 1, positions 1–6 = channel index, MSB first" is `init = 0x40 | channel`.
//!   Both registers emit sequences obeying the same recurrence; they differ in what `init` means.
//! - **`seed_channel`**: OR the channel index (the frame's `FrameInfo::channel`, or the bit
//!   chunk's `ChunkMeta::channel`) into `init` at each restart, so a hopping link (BLE under
//!   `follow_hops`) reseeds per channel. The index is the recipe's channel index; it seeds BLE
//!   correctly only where that equals the BLE channel index.
//! - **`mode`** `additive` XORs the free-running mask onto the data (a "whitener"; data-
//!   independent, so it needs the right phase: frames, or a stream from its reset).
//!   `multiplicative` is the self-synchronising descrambler: `out[k] = in[k] ⊕ Σ c_i ·
//!   in[k − (n − i)]`, the register being the last `n` *received* bits (seeded by `init`,
//!   default 0). It needs no phase and recovers `n` bits after any error or reset.
//! - **`bit_order`** `serial` (default): mask bit `k` applies to transmitted bit `k`.
//!   `byte-lsb`: each group of 8 mask bits is applied reversed, i.e. to MSB-first bytes as the
//!   register's low bit first (TI CC1101/CC2500 and Semtech PN9 byte mode). Additive only.
//! - **Reset.** On `bits` the register runs free and restarts on `DISCONTINUITY`/`RESET` (and,
//!   with `seed_channel`, on a channel change). On `frames`, bits before `offset_bits` (the sync
//!   word) pass through and `reset: per-frame` (default) restarts the register at `offset_bits`
//!   of every frame — CCSDS after each ASM, BLE and LoRa each packet. `free-running` carries
//!   the register across frame bodies (a continuously scrambled link cut into frames upstream).
//!
//! **Status (RESEARCH-012, identification).** The block also scores how well the *input* obeys
//! the polynomial's recurrence, independently of `init` or phase: the syndrome
//! `s[k] = in[k] ⊕ Σ c_i · in[k − (n − i)]` is 0 over any whitened constant run (idle fill,
//! preamble, zero padding: the mask itself obeys the recurrence) and a coin toss over whitened
//! payload or under the wrong polynomial. `error_rate` is the windowed fraction the recurrence
//! mispredicts, up to polarity (`min(r, 1 − r)`: a primitive polynomial has odd weight, so
//! constant ones give `r = 1`), `quality` is `|1 − 2r|`, and `lock` is `locked` once
//! `quality ≥ 0.8` over at least 64 checked bits. So a search can rank candidate polynomials on
//! an unknown burst. It is not a bit error rate of the decoded data. Extras: `syndrome_rate`
//! (raw `r`), `ones_fraction` (of the output). Byte-mode syndromes are computed on the
//! de-reversed byte, i.e. in mask order.
//!
//! Kernel: this generalises hk-estimate `framing::whitening` (PN9 serial and CC1101, the 7-bit
//! LFSR), which the tests hold it to bit for bit.

use hk_recipe::{BlockDescriptor, Params, PortSpec, PortType};

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::{
    P, RateMeter, drops_history, frames_io, frames_port, one_input, update_hot,
};
use crate::blocks::iq::common::{bits_in, bits_out, restarts, set_meta};
use crate::buffer::PortSlice;
use crate::registry::BuildCtx;
use crate::schema::{ParamExt, boolean, descriptor as describe, hex, int, one_of, param};
use crate::status::{Lock, Status};

/// Syndromes in the identification window.
const WINDOW: usize = 256;
/// Checked syndromes before `lock` may read `locked`.
const LOCK_MIN_CHECKED: u64 = 64;
/// `quality` at or above which the recurrence is held to fit.
const LOCK_QUALITY: f32 = 0.8;

/// The pinned `descramble` row (ADR-0011 §9.1), over the ports `mauto::planned` gives it.
pub(crate) fn descriptor(inputs: Vec<PortSpec>, outputs: Vec<PortSpec>) -> BlockDescriptor {
    describe(
        "descramble",
        "symbol",
        "LFSR de-whitening: additive (free-running, or reset per frame) or multiplicative \
         (self-synchronising); scores how well the input fits the polynomial (RESEARCH-012).",
        inputs,
        outputs,
        vec![
            param(
                "mode",
                one_of(&["additive", "multiplicative"]),
                "additive: XOR a free-running mask (whitening). multiplicative: self-synchronising \
                 descrambler over the received bits.",
            )
            .required(),
            param(
                "poly",
                hex(64),
                "Characteristic polynomial, full form: bit i = coefficient of x^i, top bit = \
                 degree n (register length), x^0 required; the x^i tap is n−i bits back. CCSDS \
                 0x1A9, PN9 0x221, BLE 0x91; a delay form 1+x^12+x^17 is 0x20021.",
            )
            .required(),
            param(
                "init",
                hex(64),
                "Register seed (low n bits). fibonacci: bit j = the j-th mask bit. Default: all \
                 ones (additive), zero (multiplicative).",
            ),
            param(
                "register",
                one_of(&["fibonacci", "galois"]),
                "Register form init refers to; galois is the BLE core spec's (additive only).",
            )
            .default_value("fibonacci"),
            param(
                "seed_channel",
                boolean(),
                "OR the channel index into init at each restart (BLE: galois, init 0x40).",
            )
            .default_value(false),
            param(
                "reset",
                one_of(&["per-frame", "free-running"]),
                "frames: restart at offset_bits of every frame, or carry the register across \
                 frames. bits: always free-running (restarts on DISCONTINUITY/RESET).",
            )
            .default_value("per-frame"),
            param(
                "offset_bits",
                int(0, 1_000_000),
                "frames: first descrambled bit (the sync word is not scrambled).",
            )
            .default_value(0),
            param(
                "bit_order",
                one_of(&["serial", "byte-lsb"]),
                "serial: mask bit k on bit k. byte-lsb: each 8 mask bits reversed onto an \
                 MSB-first byte (CC1101 PN9); additive only.",
            )
            .default_value("serial"),
        ],
        true,
    )
}

/// Builds a `descramble`.
pub(crate) fn build(params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
    Ok(Box::new(Descramble::new(params)?))
}

fn parity(x: u64) -> u8 {
    (x.count_ones() & 1) as u8
}

/// Which register `init` describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Register {
    Fibonacci,
    Galois,
}

/// The block.
pub struct Descramble {
    params: Params,
    multiplicative: bool,
    register: Register,
    /// Register length.
    n: u32,
    /// `poly` without its `x^n` term.
    taps: u64,
    /// Galois fold-back mask (`taps` bit-reversed over `n` bits).
    galois: u64,
    /// Low `n` bits.
    mask: u64,
    init: u64,
    seed_channel: bool,
    per_frame: bool,
    offset_bits: usize,
    byte_lsb: bool,
    /// Additive: the register. Multiplicative: the last `n` received bits (newest at `n−1`).
    state: u64,
    /// byte-lsb: the current group of 8 mask bits, and how many are used.
    group: [u8; 8],
    group_used: usize,
    /// Identification: input history (mask order), bits seen since restart, byte-lsb staging.
    hist: u64,
    seen: u32,
    staged: [u8; 8],
    staged_len: usize,
    meter: RateMeter,
    checked: u64,
    ones: u64,
    out_bits: u64,
    /// Channel the register was last seeded from (`None`: not seeded since reset).
    channel: Option<u16>,
    scratch: Vec<u8>,
    status: Status,
}

impl Descramble {
    fn new(params: &Params) -> Result<Self, BlockError> {
        let p = P(params);
        let bad = |m: &str| Err(BlockError::Params(m.into()));
        let multiplicative = match p.str("mode") {
            Some("additive") => false,
            Some("multiplicative") => true,
            _ => return bad("mode must be additive or multiplicative"),
        };
        let Some(poly) = p.hex("poly") else {
            return bad("poly is required");
        };
        if poly < 2 || poly & 1 == 0 {
            return bad("poly needs degree ≥ 1 and an x^0 term");
        }
        let n = 63 - poly.leading_zeros();
        if n == 0 || n > 63 {
            return bad("poly degree must be 1..=63");
        }
        let mask = (1u64 << n) - 1;
        let taps = poly & mask;
        let galois = (0..n)
            .filter(|i| taps >> i & 1 == 1)
            .fold(0u64, |m, i| m | 1 << (n - 1 - i));
        let register = match p.str("register").unwrap_or("fibonacci") {
            "fibonacci" => Register::Fibonacci,
            "galois" => Register::Galois,
            _ => return bad("register must be fibonacci or galois"),
        };
        let byte_lsb = match p.str("bit_order").unwrap_or("serial") {
            "serial" => false,
            "byte-lsb" => true,
            _ => return bad("bit_order must be serial or byte-lsb"),
        };
        let per_frame = match p.str("reset").unwrap_or("per-frame") {
            "per-frame" => true,
            "free-running" => false,
            _ => return bad("reset must be per-frame or free-running"),
        };
        let seed_channel = p.bool_or("seed_channel", false);
        if multiplicative && (register == Register::Galois || byte_lsb || seed_channel) {
            return bad(
                "multiplicative mode takes no register, bit_order or seed_channel: its register \
                 is the received bits",
            );
        }
        let init = p
            .hex("init")
            .map_or(if multiplicative { 0 } else { mask }, |v| v & mask);
        if !multiplicative && !seed_channel && init == 0 {
            return bad("additive init must be non-zero: an all-zero register never leaves zero");
        }
        let offset_bits = p.uint_or("offset_bits", 0)? as usize;
        Ok(Self {
            params: params.clone(),
            multiplicative,
            register,
            n,
            taps,
            galois,
            mask,
            init,
            seed_channel,
            per_frame,
            offset_bits,
            byte_lsb,
            state: 0,
            group: [0; 8],
            group_used: 8,
            hist: 0,
            seen: 0,
            staged: [0; 8],
            staged_len: 0,
            meter: RateMeter::new(WINDOW),
            checked: 0,
            ones: 0,
            out_bits: 0,
            channel: None,
            scratch: Vec::new(),
            status: Status::default(),
        })
    }

    /// Reseeds the register (and the identification history) for `channel`.
    fn restart(&mut self, channel: u16) {
        let seed = if self.seed_channel {
            self.init | u64::from(channel)
        } else {
            self.init
        };
        self.state = seed & self.mask;
        self.group_used = 8;
        self.hist = 0;
        self.seen = 0;
        self.staged_len = 0;
        self.channel = Some(channel);
    }

    /// One additive mask bit, serial order.
    fn step(&mut self) -> u8 {
        let out = (self.state & 1) as u8;
        self.state = match self.register {
            Register::Fibonacci => {
                (self.state >> 1) | u64::from(parity(self.state & self.taps)) << (self.n - 1)
            }
            Register::Galois => (self.state >> 1) ^ if out == 1 { self.galois } else { 0 },
        };
        out
    }

    /// Descrambles one received bit.
    fn bit(&mut self, y: u8) -> u8 {
        self.observe(y);
        let x = if self.multiplicative {
            let x = y ^ parity(self.state & self.taps);
            self.state = (self.state >> 1) | u64::from(y) << (self.n - 1);
            x
        } else if self.byte_lsb {
            if self.group_used == 8 {
                for k in 0..8 {
                    self.group[7 - k] = self.step();
                }
                self.group_used = 0;
            }
            let m = self.group[self.group_used];
            self.group_used += 1;
            y ^ m
        } else {
            y ^ self.step()
        };
        self.ones += u64::from(x);
        self.out_bits += 1;
        x
    }

    /// Feeds the identification syndrome with one received bit (mask order).
    fn observe(&mut self, y: u8) {
        if !self.byte_lsb {
            self.syndrome(y);
            return;
        }
        self.staged[self.staged_len] = y;
        self.staged_len += 1;
        if self.staged_len == 8 {
            self.staged_len = 0;
            for k in (0..8).rev() {
                self.syndrome(self.staged[k]);
            }
        }
    }

    fn syndrome(&mut self, z: u8) {
        if self.seen >= self.n {
            self.meter.push(z ^ parity(self.hist & self.taps) == 1);
            self.checked += 1;
        } else {
            self.seen += 1;
        }
        self.hist = (self.hist >> 1) | u64::from(z) << (self.n - 1);
    }

    fn publish(&mut self, items_in: u64, items_out: u64) {
        let s = &mut self.status;
        s.items_in += items_in;
        s.items_out += items_out;
        if let Some(r) = self.meter.rate() {
            let quality = (1.0 - 2.0 * r).abs();
            s.error_rate = Some(r.min(1.0 - r));
            s.quality = Some(quality);
            s.lock = if self.checked >= LOCK_MIN_CHECKED && quality >= LOCK_QUALITY {
                Lock::Locked
            } else {
                Lock::Searching
            };
            s.extra.set("syndrome_rate", f64::from(r));
        } else {
            s.lock = Lock::Searching;
        }
        if self.out_bits > 0 {
            s.extra
                .set("ones_fraction", self.ones as f64 / self.out_bits as f64);
        }
    }

    fn process_bits(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let x = bits_in(&input)?;
        let m = input.meta;
        if restarts(m.flags) {
            self.meter.clear();
            self.channel = None;
        }
        if self.channel.is_none() || (self.seed_channel && self.channel != Some(m.channel)) {
            self.restart(m.channel);
        }
        let out = io.output(0)?;
        set_meta(out, &m, m.source_index, m.source_per_item);
        let y = bits_out(out)?;
        for &b in x {
            let v = self.bit(b & 1);
            y.push(v);
        }
        let n = x.len() as u64;
        self.publish(n, n);
        Ok(())
    }

    fn process_frames(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let (meta, frames, buf) = frames_io(io)?;
        if drops_history(meta.flags) {
            self.meter.clear();
            self.channel = None;
        }
        let before = buf.len();
        let mut scratch = std::mem::take(&mut self.scratch);
        for f in frames.iter() {
            let ch = f.info.channel;
            if self.per_frame
                || self.channel.is_none()
                || (self.seed_channel && self.channel != Some(ch))
            {
                self.restart(ch);
            }
            scratch.clear();
            scratch.extend_from_slice(f.bytes);
            for i in self.offset_bits..f.info.bit_len as usize {
                let (byte, shift) = (i / 8, 7 - i % 8);
                let y = (scratch[byte] >> shift) & 1;
                let v = self.bit(y);
                scratch[byte] = (scratch[byte] & !(1 << shift)) | v << shift;
            }
            buf.push(&scratch, f.info.clone());
        }
        self.scratch = scratch;
        self.publish(frames.len() as u64, (buf.len() - before) as u64);
        Ok(())
    }
}

impl Block for Descramble {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let input = one_input("descramble", inputs, &[PortType::Bits, PortType::Frames])?;
        Ok(vec![match input.ty {
            PortType::Frames => frames_port(input, input.max_items),
            _ => PortInfo {
                hold_items: 0,
                ..input
            },
        }])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        match io.input(0)?.data {
            PortSlice::Frames(_) => self.process_frames(io),
            _ => self.process_bits(io),
        }
    }

    fn reset(&mut self) {
        self.channel = None;
        self.meter.clear();
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        update_hot(&mut self.params, params, &[], |_| {})
    }

    fn status(&self) -> Status {
        self.status
    }
}
