//! The tile file format. Little-endian throughout. Version 6 (T-377) is written; versions 1
//! (T-017), 2 (T-116), 3 (T-133), 4 (T-141) and 5 (T-332) are still read, so no migration is
//! needed: older tiles stay valid until evicted, frames of v1/v2 tiles read as of unknown source
//! and site, v1–v3 tiles carry no per-shape value counts, and v1–v4 tiles read with every
//! bias-tee state `unknown` — which is exactly what they are, since nothing recorded it.
//!
//! **Version 6 adds no bytes.** It is a *semantic* marker: a v6 level-0 tile's stored `occupancy`
//! was decided against the floor of the frame's **own** front end ([`super::FrameInput::source`]),
//! a v1–v5 one against a floor every front end that fed the pyramid contributed to. That decision
//! is baked into the cell and cannot be recomputed from the tile — the levels survive, the floor
//! they were compared with does not — so old tiles are left exactly as written and counted on
//! open as [`super::PyramidStats::tiles_pre_origin_floor`] rather than quietly credited with a
//! guarantee they do not carry. In a pyramid only one front end ever fed, the two rules give the
//! same answer, which is every single-device store.
//!
//! ```text
//! preamble (28 B)  magic "HKTILE\0\x01" [8] · format u16 · header_len u16 · payload_len u64 ·
//!                  payload_crc32 u32 · header_crc32 u32
//! header (v1)      scheme u16 · level u8 · sealed u8 · unit u8 · f_block i64 · t_block i64 ·
//!                  f_cell_hz f64 · t_cell_ns i64 · nf u32 · nt u32 · hist lo f32 · step f32 ·
//!                  bins u16 · p_low f32 · p_high f32 · provenance summary
//! header (v2)      v1 header · provenance extension: gain table (u8 present · u32) · filter
//!                  (u8 · 16 B) · spur mask (u8 · 36 B uuid text) · cell shape f32 (NaN none) ·
//!                  mixed flags u8 (1 gain table, 2 filter, 4 spur mask, 8 shape) ·
//!                  steps_dropped u64 · steps u8 · per step: t i64 · changed u8 · from state ·
//!                  to state (state = gain u8·f32·f32·u8, calibration u8·36 B, gain table u8·u32,
//!                  filter u8·16 B, spur mask u8·36 B) · payload codec u8 (0 raw, 1 zstd) ·
//!                  raw payload length u64
//! header (v3)      v2 header · origins u8 · per origin: source present u8 · source u64 · site
//!                  (u8 0 unknown, 1 unassigned, 2 mobile, 3 site + 36 B uuid text) · frames u64 ·
//!                  other_origin_frames u64 (T-133)
//! header (v4)      v3 header · cell shapes u8 · per shape: shape f32 · level-0 values u64 ·
//!                  frames u64 · other_shape_values u64 (T-141; v1–v3 tiles read with every value's shape
//!                  unrecorded, so a mixed-shape old tile gives no floor)
//! header (v5)      v4 header · bias tee u8 (0 unknown, 1 off, 2 on) · bias tee mixed u8 (T-332);
//!                  every step state also gains a trailing bias-tee u8 (see `state` above), so a
//!                  v5 step records the state either side of the change. v1–v4 read as unknown.
//! payload (v1)     observed bitmap (nt·nf bits, row-major t then f)
//!                  per observed cell: max i16 · mean i16 · p_low i16 · p_high i16 (0.01 dB,
//!                    i16::MIN = unknown) · occupancy u16 · occupancy_max u16 · coverage u16
//!                    (fractions × 65535) · frames varint
//!                  histogram bitmap (nf bits); per present row: first bin varint · len varint ·
//!                    len counts varint (leading/trailing zero bins trimmed)
//! payload (v2–v4) after zstd decompression when codec = 1: observed bitmap · then one column per
//!                  statistic over the observed cells in bitmap order (max i16[] · mean i16[] ·
//!                  p_low i16[] · p_high i16[] · occupancy u16[] · occupancy_max u16[] ·
//!                  coverage u16[] · frames varint[]) · histogram section as v1
//! ```
//!
//! The observed bitmap is the **coverage mask**: a cell absent from it was not observed, which is
//! not the same as quiet. v2 stores statistics column-wise so like bytes sit together for zstd; a
//! payload that zstd does not shrink is stored raw. The payload CRC covers the stored (possibly
//! compressed) bytes.
//!
//! A file is valid only if the magic, version, both CRCs and `28 + header_len + payload_len` =
//! file length all check (and, for zstd, the payload decompresses to exactly its raw length).
//! Writers produce `*.tile.tmp<pid>`, fsync and rename, so a torn write leaves either the old file
//! or an ignorable temp file.

use std::fs;
use std::io::{self, Read};
use std::path::Path;

use hk_model::attention::baseline::SiteKey;
use hk_model::ids::SiteId;
use hk_model::{BiasTee, CalibrationStateId, PowerUnit, SpurMaskId, TileKey, Timestamp};

use super::config::{HistogramConfig, LevelGeometry};
use super::frame::{FrontEnd, GainState, PortTag};
use super::tile::{
    FrontEndState, MAX_CELL_SHAPES, MAX_GAIN_STATES, MAX_ORIGINS, MAX_PROVENANCE_STEPS, Origin,
    ProvenanceStep, ProvenanceSummary, Tile,
};

const MAGIC: [u8; 8] = *b"HKTILE\0\x01";
/// Tile file format version written (T-116: 2; T-133: 3, per-tile origins; T-141: 4, per-shape
/// value counts; T-332: 5, bias-tee state; T-377: 6, occupancy decided against the frame's own
/// origin's floor — no new bytes, see the [module docs](self)). Versions 1–5 are still read.
///
/// **T-1024 measured a seventh and did not write it.** A per-column tag block that elided a
/// constant or duplicated plane was implemented, round-tripped and priced: it halves the *raw*
/// payload and buys **0.5 %** of the compressed level-0 tile, **−3.9 %** (i.e. worse) on a coarse
/// one, and to guarantee it never grew a tile the writer had to encode and compress twice, which
/// doubled the seal's cost on the capture thread. The planes it removes cost 0.04 B/cell of the
/// 2.83 B/cell a cell costs; see [`plane_bytes`] and `history::tests::plane_bytes`.
pub const FORMAT_VERSION: u16 = 6;
const PREAMBLE_LEN: usize = 28;
const UNKNOWN_DB: i16 = i16::MIN;
/// Largest raw payload a zstd tile may claim (bounds decompression memory).
const MAX_RAW_PAYLOAD: u64 = 1 << 31;

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE 802.3, reflected).
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// How a payload is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadCodec {
    Raw,
    Zstd,
}

/// Parsed tile header.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Header {
    pub format: u16,
    pub scheme: u16,
    pub level: u8,
    pub sealed: bool,
    pub unit: PowerUnit,
    pub f_block: i64,
    pub t_block: i64,
    pub f_cell_hz: f64,
    pub t_cell_ns: i64,
    pub nf: u32,
    pub nt: u32,
    pub hist: HistogramConfig,
    pub pct: (f32, f32),
    pub prov: ProvenanceSummary,
    pub codec: PayloadCodec,
    pub raw_payload_len: u64,
}

pub(super) fn q_db(v: f32) -> i16 {
    if v.is_finite() {
        (v * 100.0).round().clamp(-32767.0, 32767.0) as i16
    } else if v == f32::INFINITY {
        i16::MAX
    } else if v == f32::NEG_INFINITY {
        -32767
    } else {
        UNKNOWN_DB
    }
}

#[cfg(test)]
/// [`q_db`] on a coarser grid: `scale` counts steps per dB (100 = the stored 0.01 dB).
/// Measurement only ([`quant_sweep`]).
fn q_db_scale(v: f32, scale: f32) -> i16 {
    if v.is_finite() {
        (v * scale).round().clamp(-32767.0, 32767.0) as i16
    } else if v == f32::INFINITY {
        i16::MAX
    } else if v == f32::NEG_INFINITY {
        -32767
    } else {
        UNKNOWN_DB
    }
}

pub(super) fn dq_db(v: i16) -> f32 {
    if v == UNKNOWN_DB {
        f32::NAN
    } else {
        f32::from(v) / 100.0
    }
}

fn q_frac(v: f64) -> u16 {
    if v.is_finite() {
        (v.clamp(0.0, 1.0) * 65535.0).round() as u16
    } else {
        0
    }
}

fn dq_frac(v: u16) -> f32 {
    f32::from(v) / 65535.0
}

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(s)
    }
    fn arr<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.arr()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.arr()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.arr()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.arr()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_le_bytes(self.arr()?))
    }
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    fn uuid_text<T: std::str::FromStr>(&mut self) -> Option<Option<T>> {
        if self.u8()? != 1 {
            return Some(None);
        }
        let s = std::str::from_utf8(self.take(36)?).ok()?;
        Some(Some(s.parse().ok()?))
    }
    fn opt_u32(&mut self) -> Option<Option<u32>> {
        let present = self.u8()? != 0;
        let v = self.u32()?;
        Some(present.then_some(v))
    }
    fn opt_tag(&mut self) -> Option<Option<PortTag>> {
        let present = self.u8()? != 0;
        let b = self.arr::<16>()?;
        Some(present.then(|| PortTag::from_bytes(b)))
    }
    fn site(&mut self) -> Option<Option<SiteKey>> {
        Some(match self.u8()? {
            0 => None,
            1 => Some(SiteKey::Unassigned),
            2 => Some(SiteKey::Mobile),
            3 => {
                let s = std::str::from_utf8(self.take(36)?).ok()?;
                Some(SiteKey::Site(s.parse::<SiteId>().ok()?))
            }
            _ => return None,
        })
    }
}

fn put_site(buf: &mut Vec<u8>, site: Option<SiteKey>) {
    match site {
        None => buf.push(0),
        Some(SiteKey::Unassigned) => buf.push(1),
        Some(SiteKey::Mobile) => buf.push(2),
        Some(SiteKey::Site(id)) => {
            buf.push(3);
            buf.extend_from_slice(id.to_string().as_bytes()); // 36 bytes
        }
    }
}

fn put_uuid_text(buf: &mut Vec<u8>, id: Option<impl ToString>) {
    match id {
        Some(id) => {
            buf.push(1);
            buf.extend_from_slice(id.to_string().as_bytes()); // 36 bytes
        }
        None => buf.push(0),
    }
}

fn put_opt_u32(buf: &mut Vec<u8>, v: Option<u32>) {
    buf.push(u8::from(v.is_some()));
    buf.extend_from_slice(&v.unwrap_or(0).to_le_bytes());
}

fn put_opt_tag(buf: &mut Vec<u8>, v: Option<PortTag>) {
    buf.push(u8::from(v.is_some()));
    buf.extend_from_slice(&v.unwrap_or_default().bytes());
}

/// The bias-tee state as one byte. `Unknown` is 0 so a zero byte — and a v1–v4 file, which has no
/// byte at all — reads as unknown rather than as a claim that the DC was off (T-325).
fn bias_byte(b: BiasTee) -> u8 {
    match b {
        BiasTee::Unknown => 0,
        BiasTee::Off => 1,
        BiasTee::On => 2,
    }
}

/// Decodes [`bias_byte`]. An unrecognised byte is refused rather than read as unknown: a corrupt
/// file must not silently become a state.
fn bias_from(v: u8) -> Option<BiasTee> {
    match v {
        0 => Some(BiasTee::Unknown),
        1 => Some(BiasTee::Off),
        2 => Some(BiasTee::On),
        _ => None,
    }
}

/// `bias` writes the T-332 trailing bias-tee byte (format 5 and the v2 source-state file); the
/// format-2..4 writer kept for the backward-read test passes `false` so its bytes are unchanged.
fn put_state(buf: &mut Vec<u8>, s: &FrontEndState, bias: bool) {
    let g = s.gain.unwrap_or_default();
    buf.push(u8::from(s.gain.is_some()));
    buf.extend_from_slice(&g.lna_db.to_le_bytes());
    buf.extend_from_slice(&g.vga_db.to_le_bytes());
    buf.push(u8::from(g.amp_on));
    put_uuid_text(buf, s.calibration);
    put_opt_u32(buf, s.front_end.gain_table);
    put_opt_tag(buf, s.front_end.filter);
    put_uuid_text(buf, s.front_end.spur_mask);
    if bias {
        buf.push(bias_byte(s.bias_tee));
    }
}

fn get_state(c: &mut Cur<'_>, bias: bool) -> Option<FrontEndState> {
    let present = c.u8()? != 0;
    let g = GainState {
        lna_db: c.f32()?,
        vga_db: c.f32()?,
        amp_on: c.u8()? != 0,
    };
    let calibration = c.uuid_text::<CalibrationStateId>()?;
    let front_end = FrontEnd {
        gain_table: c.opt_u32()?,
        filter: c.opt_tag()?,
        spur_mask: c.uuid_text::<SpurMaskId>()?,
    };
    Some(FrontEndState {
        gain: present.then_some(g),
        calibration,
        // A state written before T-332 recorded nothing about the DC, so it is unknown.
        bias_tee: if bias {
            bias_from(c.u8()?)?
        } else {
            BiasTee::Unknown
        },
        front_end,
    })
}

const SOURCE_STATE_MAGIC: &[u8; 4] = b"HKFS";
/// Source-state file version written (T-126: 1; T-332: 2, the bias-tee byte). Version 1 is read.
const SOURCE_STATE_VERSION: u8 = 2;

/// The per-source front-end state file (T-126): `"HKFS" · version u8 · count u32 · count ×
/// (source u64 · state · shape present u8 · shape f32) · crc32 u32` of everything before it.
/// Version 2 (T-332) writes the bias-tee byte in each state; version 1 files are still read, with
/// every state's bias tee unknown.
pub(super) fn encode_source_states(states: &[(u64, FrontEndState, Option<f32>)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16 + states.len() * 128);
    buf.extend_from_slice(SOURCE_STATE_MAGIC);
    buf.push(SOURCE_STATE_VERSION);
    buf.extend_from_slice(&(states.len() as u32).to_le_bytes());
    for (source, state, shape) in states {
        buf.extend_from_slice(&source.to_le_bytes());
        put_state(&mut buf, state, true);
        buf.push(u8::from(shape.is_some()));
        buf.extend_from_slice(&shape.unwrap_or(0.0).to_le_bytes());
    }
    let crc = crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

/// Decodes [`encode_source_states`]; `None` if truncated, corrupt or of another version.
pub(super) fn decode_source_states(b: &[u8]) -> Option<Vec<(u64, FrontEndState, Option<f32>)>> {
    let body = b.len().checked_sub(4)?;
    if crc32(&b[..body]).to_le_bytes() != b[body..] {
        return None;
    }
    let mut c = Cur {
        b: &b[..body],
        p: 0,
    };
    if c.take(4)? != SOURCE_STATE_MAGIC {
        return None;
    }
    let version = c.u8()?;
    if !(1..=SOURCE_STATE_VERSION).contains(&version) {
        return None;
    }
    let bias = version >= 2;
    let n = c.u32()?;
    let mut out = Vec::with_capacity(n.min(1 << 16) as usize);
    for _ in 0..n {
        let source = c.u64()?;
        let state = get_state(&mut c, bias)?;
        let present = c.u8()? != 0;
        let shape = c.f32()?;
        out.push((source, state, present.then_some(shape)));
    }
    (c.p == body).then_some(out)
}

fn encode_header(h: &Header, buf: &mut Vec<u8>) {
    buf.extend_from_slice(&h.scheme.to_le_bytes());
    buf.push(h.level);
    buf.push(u8::from(h.sealed));
    buf.push(match h.unit {
        PowerUnit::Dbfs => 0,
        PowerUnit::Dbm => 1,
    });
    buf.extend_from_slice(&h.f_block.to_le_bytes());
    buf.extend_from_slice(&h.t_block.to_le_bytes());
    buf.extend_from_slice(&h.f_cell_hz.to_le_bytes());
    buf.extend_from_slice(&h.t_cell_ns.to_le_bytes());
    buf.extend_from_slice(&h.nf.to_le_bytes());
    buf.extend_from_slice(&h.nt.to_le_bytes());
    buf.extend_from_slice(&h.hist.lo_db.to_le_bytes());
    buf.extend_from_slice(&h.hist.step_db.to_le_bytes());
    buf.extend_from_slice(&h.hist.bins.to_le_bytes());
    buf.extend_from_slice(&h.pct.0.to_le_bytes());
    buf.extend_from_slice(&h.pct.1.to_le_bytes());
    let p = &h.prov;
    for v in [
        p.frames,
        p.suspect_frames,
        p.dropped_samples,
        p.gain_changes,
        p.other_gain_frames,
        p.unknown_gain_frames,
    ] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf.push(p.gain_states.len() as u8);
    for (g, n) in &p.gain_states {
        buf.extend_from_slice(&g.lna_db.to_le_bytes());
        buf.extend_from_slice(&g.vga_db.to_le_bytes());
        buf.push(u8::from(g.amp_on));
        buf.extend_from_slice(&n.to_le_bytes());
    }
    put_uuid_text(buf, p.calibration);
    buf.push(u8::from(p.calibration_mixed));
    for t in [p.first_frame, p.last_frame] {
        buf.push(u8::from(t.is_some()));
        buf.extend_from_slice(&t.map_or(0, Timestamp::as_unix_nanos).to_le_bytes());
    }
    if h.format < 2 {
        return;
    }
    put_opt_u32(buf, p.gain_table);
    put_opt_tag(buf, p.filter);
    put_uuid_text(buf, p.spur_mask);
    buf.extend_from_slice(&p.cell_shape.unwrap_or(f32::NAN).to_le_bytes());
    let flags = u8::from(p.gain_table_mixed)
        | u8::from(p.filter_mixed) << 1
        | u8::from(p.spur_mask_mixed) << 2
        | u8::from(p.cell_shape_mixed) << 3;
    buf.push(flags);
    buf.extend_from_slice(&p.steps_dropped.to_le_bytes());
    let n = p.steps.len().min(MAX_PROVENANCE_STEPS);
    buf.push(n as u8);
    for s in &p.steps[..n] {
        buf.extend_from_slice(&s.t.as_unix_nanos().to_le_bytes());
        buf.push(s.changed);
        put_state(buf, &s.from, h.format >= 5);
        put_state(buf, &s.to, h.format >= 5);
    }
    buf.push(match h.codec {
        PayloadCodec::Raw => 0,
        PayloadCodec::Zstd => 1,
    });
    buf.extend_from_slice(&h.raw_payload_len.to_le_bytes());
    if h.format < 3 {
        return;
    }
    let n = p.origins.len().min(MAX_ORIGINS);
    buf.push(n as u8);
    for (o, frames) in &p.origins[..n] {
        buf.push(u8::from(o.source.is_some()));
        buf.extend_from_slice(&o.source.unwrap_or(0).to_le_bytes());
        put_site(buf, o.site);
        buf.extend_from_slice(&frames.to_le_bytes());
    }
    buf.extend_from_slice(&p.other_origin_frames.to_le_bytes());
    if h.format < 4 {
        return;
    }
    let n = p.cell_shapes.len().min(MAX_CELL_SHAPES);
    buf.push(n as u8);
    for (shape, values, frames) in &p.cell_shapes[..n] {
        buf.extend_from_slice(&shape.to_le_bytes());
        buf.extend_from_slice(&values.to_le_bytes());
        buf.extend_from_slice(&frames.to_le_bytes());
    }
    let dropped = p.cell_shapes[n..]
        .iter()
        .fold(0u64, |acc, &(_, v, _)| acc.saturating_add(v));
    buf.extend_from_slice(&p.other_shape_values.saturating_add(dropped).to_le_bytes());
    if h.format < 5 {
        return;
    }
    buf.push(bias_byte(p.bias_tee));
    buf.push(u8::from(p.bias_tee_mixed));
}

fn decode_header(c: &mut Cur<'_>, format: u16) -> Option<Header> {
    let scheme = c.u16()?;
    let level = c.u8()?;
    let sealed = c.u8()? != 0;
    let unit = match c.u8()? {
        0 => PowerUnit::Dbfs,
        1 => PowerUnit::Dbm,
        _ => return None,
    };
    let f_block = c.i64()?;
    let t_block = c.i64()?;
    let f_cell_hz = c.f64()?;
    let t_cell_ns = c.i64()?;
    let nf = c.u32()?;
    let nt = c.u32()?;
    let hist = HistogramConfig {
        lo_db: c.f32()?,
        step_db: c.f32()?,
        bins: c.u16()?,
    };
    let pct = (c.f32()?, c.f32()?);
    let mut p = ProvenanceSummary {
        frames: c.u64()?,
        suspect_frames: c.u64()?,
        dropped_samples: c.u64()?,
        gain_changes: c.u64()?,
        other_gain_frames: c.u64()?,
        unknown_gain_frames: c.u64()?,
        ..Default::default()
    };
    let n_gain = c.u8()? as usize;
    if n_gain > MAX_GAIN_STATES {
        return None;
    }
    p.gain_states.reserve_exact(MAX_GAIN_STATES);
    for _ in 0..n_gain {
        let g = GainState {
            lna_db: c.f32()?,
            vga_db: c.f32()?,
            amp_on: c.u8()? != 0,
        };
        p.gain_states.push((g, c.u64()?));
    }
    p.calibration = c.uuid_text()?;
    p.calibration_mixed = c.u8()? != 0;
    let mut times = [None, None];
    for t in &mut times {
        let present = c.u8()? != 0;
        let ns = c.i64()?;
        *t = present.then(|| Timestamp::from_unix_nanos(ns));
    }
    [p.first_frame, p.last_frame] = times;
    let (mut codec, mut raw_payload_len) = (PayloadCodec::Raw, 0);
    if format >= 2 {
        p.gain_table = c.opt_u32()?;
        p.filter = c.opt_tag()?;
        p.spur_mask = c.uuid_text()?;
        let shape = c.f32()?;
        p.cell_shape = shape.is_finite().then_some(shape);
        let flags = c.u8()?;
        p.gain_table_mixed = flags & 1 != 0;
        p.filter_mixed = flags & 2 != 0;
        p.spur_mask_mixed = flags & 4 != 0;
        p.cell_shape_mixed = flags & 8 != 0;
        p.steps_dropped = c.u64()?;
        let n = c.u8()? as usize;
        if n > MAX_PROVENANCE_STEPS {
            return None;
        }
        p.steps.reserve_exact(MAX_PROVENANCE_STEPS);
        for _ in 0..n {
            let t = Timestamp::from_unix_nanos(c.i64()?);
            let changed = c.u8()?;
            let from = get_state(c, format >= 5)?;
            let to = get_state(c, format >= 5)?;
            p.steps.push(ProvenanceStep {
                t,
                changed,
                from,
                to,
            });
        }
        codec = match c.u8()? {
            0 => PayloadCodec::Raw,
            1 => PayloadCodec::Zstd,
            _ => return None,
        };
        raw_payload_len = c.u64()?;
    }
    p.origins.reserve_exact(MAX_ORIGINS);
    if format >= 3 {
        let n = c.u8()? as usize;
        if n > MAX_ORIGINS {
            return None;
        }
        for _ in 0..n {
            let present = c.u8()? != 0;
            let source = c.u64()?;
            let site = c.site()?;
            let frames = c.u64()?;
            p.origins.push((
                Origin {
                    source: present.then_some(source),
                    site,
                },
                frames,
            ));
        }
        p.other_origin_frames = c.u64()?;
    } else if p.frames > 0 {
        // Before T-133 tiles did not record where frames came from.
        p.origins.push((Origin::UNKNOWN, p.frames));
    }
    p.cell_shapes.reserve_exact(MAX_CELL_SHAPES);
    if format >= 4 {
        let n = c.u8()? as usize;
        if n > MAX_CELL_SHAPES {
            return None;
        }
        for _ in 0..n {
            let shape = c.f32()?;
            let values = c.u64()?;
            let frames = c.u64()?;
            if !(shape.is_finite() && shape > 0.0) {
                return None;
            }
            p.cell_shapes.push((shape, values, frames));
        }
        p.other_shape_values = c.u64()?;
    } else if p.frames > 0 {
        // Before T-141 tiles did not count values per shape: a mixed old tile has no mixture.
        p.other_shape_values = p.frames;
    }
    if format >= 5 {
        p.bias_tee = bias_from(c.u8()?)?;
        p.bias_tee_mixed = c.u8()? != 0;
    }
    // Before T-332 nothing recorded the bias tee, so v1–v4 keep the `Unknown` default: unknown is
    // what they are, and reading it as off would claim a comparability they never evidenced.
    Some(Header {
        format,
        scheme,
        level,
        sealed,
        unit,
        f_block,
        t_block,
        f_cell_hz,
        t_cell_ns,
        nf,
        nt,
        hist,
        pct,
        prov: p,
        codec,
        raw_payload_len,
    })
}

fn encode_histograms(tile: &Tile, buf: &mut Vec<u8>) {
    let hbitmap_at = buf.len();
    buf.resize(hbitmap_at + tile.nf.div_ceil(8), 0);
    for f in 0..tile.nf {
        let row = tile.hist_row(f);
        let Some(first) = row.iter().position(|&c| c > 0) else {
            continue;
        };
        let last = row.iter().rposition(|&c| c > 0).unwrap_or(first);
        buf[hbitmap_at + f / 8] |= 1 << (f % 8);
        put_varint(buf, first as u64);
        put_varint(buf, (last - first + 1) as u64);
        for &c in &row[first..=last] {
            put_varint(buf, u64::from(c));
        }
    }
}

/// The per-cell statistic columns, in stored order. `frames` is a varint column and is not one
/// of these; the histogram section and the observed bitmap are not columns at all.
pub(crate) const N_COLS: usize = 7;
/// Plane names for the byte-cost table ([`plane_bytes`]), in stored order.
#[cfg(test)]
pub(crate) const PLANE_NAMES: [&str; N_COLS + 3] = [
    "max_db",
    "mean_db",
    "p_low_db",
    "p_high_db",
    "occupancy",
    "occupancy_max",
    "coverage",
    "frames",
    "histogram",
    "observed bitmap",
];
/// Plane index of the varint `frames` column, of the histogram section and of the bitmap.
pub(crate) const PLANE_FRAMES: usize = N_COLS;
pub(crate) const PLANE_HIST: usize = N_COLS + 1;
#[cfg(test)]
pub(crate) const PLANE_BITMAP: usize = N_COLS + 2;

/// Appends column `c`'s u16 values, one per observed cell, in bitmap order.
fn write_col(tile: &Tile, c: usize, buf: &mut Vec<u8>) {
    let n = tile.nf * tile.nt;
    let cells = (0..n).filter(|&i| tile.count[i] > 0);
    match c {
        0 => cells.for_each(|i| buf.extend_from_slice(&q_db(tile.max[i]).to_le_bytes())),
        1 => cells.for_each(|i| buf.extend_from_slice(&q_db(tile.mean_db(i)).to_le_bytes())),
        2 => cells.for_each(|i| buf.extend_from_slice(&q_db(tile.p_lo[i]).to_le_bytes())),
        3 => cells.for_each(|i| buf.extend_from_slice(&q_db(tile.p_hi[i]).to_le_bytes())),
        4 => cells.for_each(|i| {
            let (obs, occ) = tile.cell_obs(i);
            let ratio = if obs > 0.0 { occ / obs } else { 0.0 };
            buf.extend_from_slice(&q_frac(ratio).to_le_bytes());
        }),
        5 => cells.for_each(|i| {
            buf.extend_from_slice(&q_frac(f64::from(tile.occ_max[i])).to_le_bytes());
        }),
        _ => cells.for_each(|i| {
            let (obs, _) = tile.cell_obs(i);
            buf.extend_from_slice(&q_frac(obs / tile.t_cell_s).to_le_bytes());
        }),
    }
}

/// The raw payload: observed bitmap, statistic columns, `frames` varints, histograms.
///
/// `skip` is a plane bitmask used **only** by [`plane_bytes`] to price one plane by leaving it
/// out; production always passes 0, and a payload written with a non-zero mask is not readable.
fn encode_payload_masked(tile: &Tile, buf: &mut Vec<u8>, skip: u16) {
    buf.clear();
    let n = tile.nf * tile.nt;
    buf.resize(n.div_ceil(8), 0);
    let mut observed = 0usize;
    for i in 0..n {
        if tile.count[i] > 0 {
            buf[i / 8] |= 1 << (i % 8);
            observed += 1;
        }
    }
    buf.reserve(observed * 17 + tile.nf * 8);
    for c in 0..N_COLS {
        if skip & (1 << c) == 0 {
            write_col(tile, c, buf);
        }
    }
    if skip & (1 << PLANE_FRAMES) == 0 {
        for i in (0..n).filter(|&i| tile.count[i] > 0) {
            put_varint(buf, u64::from(tile.count[i]));
        }
    }
    if skip & (1 << PLANE_HIST) == 0 {
        encode_histograms(tile, buf);
    }
}

fn encode_payload(tile: &Tile, buf: &mut Vec<u8>) {
    encode_payload_masked(tile, buf, 0);
}

/// **T-585: one committed row footprint in stored form.** Encodes cells `[lo, hi)` of `src` into
/// `buf` (cleared first) exactly as the tile payload stores a cell — observed bitmap, then one
/// column per statistic over the observed cells in bitmap order: `max` i16 · `mean` i16 ·
/// `occupancy` u16 · `occupancy_max` u16 · `coverage` u16 · `frames` varint — minus the two
/// percentile columns, which no row fold ever writes (`docs/16` §6.2). Quantised to the same
/// grid, so re-encoding the decoded cells at the seal is idempotent.
///
/// This form never reaches disk. It is what a coarse node holds a committed row as while the
/// tile it belongs to is still filling, so the node's residency is one row of accumulator and not
/// `nt` of them ([`super::live::LiveTile`]).
pub(crate) fn encode_row_cells(
    src: &super::tile::RowSrc<'_>,
    lo: usize,
    hi: usize,
    buf: &mut Vec<u8>,
) {
    buf.clear();
    let m = hi.saturating_sub(lo);
    buf.resize(m.div_ceil(8), 0);
    let mut observed = 0usize;
    for (j, f) in (lo..hi).enumerate() {
        if src.count[f] > 0 {
            buf[j / 8] |= 1 << (j % 8);
            observed += 1;
        }
    }
    buf.reserve(observed * 13);
    let cells = || (lo..hi).filter(|&f| src.count[f] > 0);
    for f in cells() {
        buf.extend_from_slice(&q_db(src.max[f]).to_le_bytes());
    }
    for f in cells() {
        let mean = super::stats::db(src.sum_lin[f] / f64::from(src.count[f]));
        buf.extend_from_slice(&q_db(mean).to_le_bytes());
    }
    for f in cells() {
        let (obs, occ) = src.cell_obs(f);
        let ratio = if obs > 0.0 { occ / obs } else { 0.0 };
        buf.extend_from_slice(&q_frac(ratio).to_le_bytes());
    }
    for f in cells() {
        buf.extend_from_slice(&q_frac(f64::from(src.occ_max[f])).to_le_bytes());
    }
    for f in cells() {
        let (obs, _) = src.cell_obs(f);
        buf.extend_from_slice(&q_frac(obs / src.t_cell_s).to_le_bytes());
    }
    for f in cells() {
        put_varint(buf, u64::from(src.count[f]));
    }
}

/// [`encode_row_cells`]'s inverse over `m` cells: calls `cell(j, frames, max_db, mean_db,
/// occupancy, occupancy_max, coverage)` for each observed cell `j` in `0..m`. `None` if the
/// bytes do not parse, in which case nothing (or a prefix) was delivered.
pub(crate) fn decode_row_cells(
    bytes: &[u8],
    m: usize,
    mut cell: impl FnMut(usize, u32, f32, f32, f32, f32, f32),
) -> Option<()> {
    let mut c = Cur { b: bytes, p: 0 };
    let bitmap = c.take(m.div_ceil(8))?;
    let observed = |j: usize| bitmap[j / 8] & (1 << (j % 8)) != 0;
    let k = (0..m).filter(|&j| observed(j)).count();
    let cols: Vec<&[u8]> = (0..5)
        .map(|_| c.take(k.checked_mul(2)?))
        .collect::<Option<_>>()?;
    let at = |col: usize, i: usize| [cols[col][2 * i], cols[col][2 * i + 1]];
    for (i, j) in (0..m).filter(|&j| observed(j)).enumerate() {
        let count = u32::try_from(c.varint()?).ok()?;
        cell(
            j,
            count,
            dq_db(i16::from_le_bytes(at(0, i))),
            dq_db(i16::from_le_bytes(at(1, i))),
            dq_frac(u16::from_le_bytes(at(2, i))),
            dq_frac(u16::from_le_bytes(at(3, i))),
            dq_frac(u16::from_le_bytes(at(4, i))),
        );
    }
    (c.p == bytes.len()).then_some(())
}

#[cfg(test)]
/// Reads a tile file whose geometry is taken from its own header — the only way to read a store
/// directory without the [`super::PyramidConfig`] that wrote it. Returns the header, the tile and
/// the file length.
pub(crate) fn read_tile_standalone(path: &Path) -> io::Result<Option<(Header, Tile, u64)>> {
    let bytes = fs::read(path)?;
    let Some(pre) = parse_preamble(&bytes) else {
        return Ok(None);
    };
    let hb = bytes
        .get(PREAMBLE_LEN..PREAMBLE_LEN + pre.header_len)
        .unwrap_or_default();
    let Some(h) = decode_header(&mut Cur { b: hb, p: 0 }, pre.format) else {
        return Ok(None);
    };
    let g = LevelGeometry {
        f_cell_hz: h.f_cell_hz,
        t_cell_ns: h.t_cell_ns,
        nt: h.nt as usize,
        f_factor: 1,
        t_factor: 1,
        from: None,
    };
    let len = bytes.len() as u64;
    Ok(decode_bytes(&bytes, &g, usize::from(h.hist.bins)).map(|t| (h, t, len)))
}

#[cfg(test)]
/// The compressed and raw payload with the planes in `skip` left out — what a *group* of planes
/// costs together, which is the only honest question when planes duplicate one another.
/// Measurement only.
pub(crate) fn group_bytes(tile: &Tile, skip: u16, compression: i32) -> (usize, usize) {
    let mut b = Vec::new();
    encode_payload_masked(tile, &mut b, skip);
    let z = zstd::bulk::compress(&b, compression).map_or(b.len(), |c| c.len());
    (b.len(), z)
}

#[cfg(test)]
/// Compresses `tile`'s format-`format` payload at each of `levels`, returning
/// `(level, compressed bytes, seconds to compress, seconds to decompress)`. Measurement only.
pub(crate) fn payload_sweep(tile: &Tile, levels: &[i32]) -> Vec<(i32, usize, f64, f64)> {
    let mut raw = Vec::new();
    encode_payload_masked(tile, &mut raw, 0);
    levels
        .iter()
        .map(|&l| {
            let t = std::time::Instant::now();
            let c = zstd::bulk::compress(&raw, l).unwrap_or_default();
            let enc = t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let _ = zstd::bulk::decompress(&c, raw.len());
            (l, c.len(), enc, t.elapsed().as_secs_f64())
        })
        .collect()
}

#[cfg(test)]
/// **What the 0.01 dB grid costs.** Re-quantises the four dB planes to each `step_db` and
/// returns `(step_db, compressed payload bytes)`. Measurement only, and deliberately *not* a
/// knob: a coarser grid discards measured values, however far below the estimator's own standard
/// error they are, and that is the user's call, not the codec's (T-1024).
pub(crate) fn quant_sweep(tile: &Tile, steps: &[f32], compression: i32) -> Vec<(f32, usize)> {
    let n = tile.nf * tile.nt;
    let cells = || (0..n).filter(|&i| tile.count[i] > 0);
    steps
        .iter()
        .map(|&step| {
            let scale = 1.0 / step;
            let mut b = vec![0u8; n.div_ceil(8)];
            for i in 0..n {
                if tile.count[i] > 0 {
                    b[i / 8] |= 1 << (i % 8);
                }
            }
            for get in [
                &(|t: &Tile, i: usize| t.max[i]) as &dyn Fn(&Tile, usize) -> f32,
                &|t: &Tile, i: usize| t.mean_db(i),
                &|t: &Tile, i: usize| t.p_lo[i],
                &|t: &Tile, i: usize| t.p_hi[i],
            ] {
                for i in cells() {
                    b.extend_from_slice(&q_db_scale(get(tile, i), scale).to_le_bytes());
                }
            }
            for c in 4..N_COLS {
                write_col(tile, c, &mut b);
            }
            for i in cells() {
                put_varint(&mut b, u64::from(tile.count[i]));
            }
            encode_histograms(tile, &mut b);
            let z = zstd::bulk::compress(&b, compression).map_or(b.len(), |c| c.len());
            (step, z)
        })
        .collect()
}

#[cfg(test)]
/// What one plane of a tile costs on disk (T-1024, the disk half of T-1019).
#[derive(Clone, Debug)]
pub(crate) struct PlaneCost {
    pub name: &'static str,
    /// Bytes this plane occupies in the uncompressed payload.
    pub raw: usize,
    /// Bytes this plane compresses to **on its own** — what it would cost if nothing else in the
    /// tile helped it.
    pub alone: usize,
    /// Bytes the compressed tile **grows by** because this plane is in it: the whole payload
    /// compressed, minus the same payload compressed without the plane. This is the number that
    /// says what dropping the plane would buy, and it is not the same as `alone` — zstd prices a
    /// column against its neighbours.
    pub marginal: i64,
}

#[cfg(test)]
/// Prices every plane of `tile` at `format`, compressed at `compression`. Returns the observed
/// cell count, the raw and compressed payload sizes, and one [`PlaneCost`] per plane.
///
/// Measurement only: it encodes the payload ten times over and is never on the capture thread.
pub(crate) fn plane_bytes(tile: &Tile, compression: i32) -> (usize, usize, usize, Vec<PlaneCost>) {
    let z = |b: &[u8]| zstd::bulk::compress(b, compression).map_or(b.len(), |c| c.len());
    let mut full = Vec::new();
    encode_payload_masked(tile, &mut full, 0);
    let z_full = z(&full);
    let n = tile.nf * tile.nt;
    let cells = (0..n).filter(|&i| tile.count[i] > 0).count();
    let mut scratch = Vec::new();
    let mut out = Vec::with_capacity(PLANE_NAMES.len());
    for (p, name) in PLANE_NAMES.iter().enumerate() {
        if p == PLANE_BITMAP {
            let bytes = &full[..n.div_ceil(8)];
            out.push(PlaneCost {
                name,
                raw: bytes.len(),
                alone: z(bytes),
                marginal: 0, // The bitmap is the coverage mask: nothing else says which cells exist.
            });
            continue;
        }
        let mut without = Vec::new();
        encode_payload_masked(tile, &mut without, 1 << p);
        scratch.clear();
        match p {
            PLANE_FRAMES => {
                for i in (0..n).filter(|&i| tile.count[i] > 0) {
                    put_varint(&mut scratch, u64::from(tile.count[i]));
                }
            }
            PLANE_HIST => encode_histograms(tile, &mut scratch),
            c => write_col(tile, c, &mut scratch),
        }
        out.push(PlaneCost {
            name,
            raw: full.len() - without.len(),
            alone: z(&scratch),
            marginal: z_full as i64 - z(&without) as i64,
        });
    }
    (cells, full.len(), z_full, out)
}

/// Serialises `tile` into `buf` (cleared first) as format [`FORMAT_VERSION`], compressing the
/// payload with zstd at `compression` when that shrinks it. `payload` is scratch. Returns the size
/// the file would have had with a raw payload (for the compression-ratio counter).
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode(
    tile: &Tile,
    sealed: bool,
    unit: PowerUnit,
    g: &LevelGeometry,
    hist: &HistogramConfig,
    pct: (f32, f32),
    compression: Option<i32>,
    buf: &mut Vec<u8>,
    payload: &mut Vec<u8>,
) -> u64 {
    encode_format(
        FORMAT_VERSION,
        tile,
        sealed,
        unit,
        g,
        hist,
        pct,
        compression,
        buf,
        payload,
    )
}

/// [`encode`] as `format` (2 or [`FORMAT_VERSION`]; the older writer is kept for the
/// backward-read tests).
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_format(
    format: u16,
    tile: &Tile,
    sealed: bool,
    unit: PowerUnit,
    g: &LevelGeometry,
    hist: &HistogramConfig,
    pct: (f32, f32),
    compression: Option<i32>,
    buf: &mut Vec<u8>,
    payload: &mut Vec<u8>,
) -> u64 {
    debug_assert!((2..=FORMAT_VERSION).contains(&format));
    encode_payload(tile, payload);
    let raw_len = payload.len();
    let compressed = compression
        .and_then(|level| zstd::bulk::compress(payload, level).ok())
        .filter(|c| c.len() < raw_len);
    let codec = if compressed.is_some() {
        PayloadCodec::Zstd
    } else {
        PayloadCodec::Raw
    };
    buf.clear();
    buf.resize(PREAMBLE_LEN, 0);
    let header = Header {
        format,
        scheme: tile.key.scheme,
        level: tile.key.level,
        sealed,
        unit,
        f_block: tile.key.f_block,
        t_block: tile.key.t_block,
        f_cell_hz: g.f_cell_hz,
        t_cell_ns: g.t_cell_ns,
        nf: tile.nf as u32,
        nt: tile.nt as u32,
        hist: *hist,
        pct,
        prov: tile.prov.clone(),
        codec,
        raw_payload_len: raw_len as u64,
    };
    encode_header(&header, buf);
    let header_end = buf.len();
    buf.extend_from_slice(compressed.as_deref().unwrap_or(payload));
    finish_preamble(buf, format, header_end);
    (header_end + raw_len) as u64
}

fn finish_preamble(buf: &mut [u8], format: u16, header_end: usize) {
    let header_len = (header_end - PREAMBLE_LEN) as u16;
    let payload_len = (buf.len() - header_end) as u64;
    let payload_crc = crc32(&buf[header_end..]);
    let header_crc = crc32(&buf[PREAMBLE_LEN..header_end]);
    let pre = &mut buf[..PREAMBLE_LEN];
    pre[0..8].copy_from_slice(&MAGIC);
    pre[8..10].copy_from_slice(&format.to_le_bytes());
    pre[10..12].copy_from_slice(&header_len.to_le_bytes());
    pre[12..20].copy_from_slice(&payload_len.to_le_bytes());
    pre[20..24].copy_from_slice(&payload_crc.to_le_bytes());
    pre[24..28].copy_from_slice(&header_crc.to_le_bytes());
}

/// The T-017 version-1 writer, kept for the backward-read test.
#[cfg(test)]
pub(crate) fn encode_v1(
    tile: &Tile,
    sealed: bool,
    unit: PowerUnit,
    g: &LevelGeometry,
    hist: &HistogramConfig,
    pct: (f32, f32),
    buf: &mut Vec<u8>,
) {
    buf.clear();
    buf.resize(PREAMBLE_LEN, 0);
    let header = Header {
        format: 1,
        scheme: tile.key.scheme,
        level: tile.key.level,
        sealed,
        unit,
        f_block: tile.key.f_block,
        t_block: tile.key.t_block,
        f_cell_hz: g.f_cell_hz,
        t_cell_ns: g.t_cell_ns,
        nf: tile.nf as u32,
        nt: tile.nt as u32,
        hist: *hist,
        pct,
        prov: tile.prov.clone(),
        codec: PayloadCodec::Raw,
        raw_payload_len: 0,
    };
    encode_header(&header, buf);
    let header_end = buf.len();
    let n = tile.nf * tile.nt;
    let bitmap_at = buf.len();
    buf.resize(bitmap_at + n.div_ceil(8), 0);
    for i in 0..n {
        if tile.count[i] == 0 {
            continue;
        }
        buf[bitmap_at + i / 8] |= 1 << (i % 8);
        let (obs, occ) = tile.cell_obs(i);
        buf.extend_from_slice(&q_db(tile.max[i]).to_le_bytes());
        buf.extend_from_slice(&q_db(tile.mean_db(i)).to_le_bytes());
        buf.extend_from_slice(&q_db(tile.p_lo[i]).to_le_bytes());
        buf.extend_from_slice(&q_db(tile.p_hi[i]).to_le_bytes());
        let ratio = if obs > 0.0 { occ / obs } else { 0.0 };
        buf.extend_from_slice(&q_frac(ratio).to_le_bytes());
        buf.extend_from_slice(&q_frac(f64::from(tile.occ_max[i])).to_le_bytes());
        buf.extend_from_slice(&q_frac(obs / tile.t_cell_s).to_le_bytes());
        put_varint(buf, u64::from(tile.count[i]));
    }
    let mut hist_buf = Vec::new();
    encode_histograms(tile, &mut hist_buf);
    buf.extend_from_slice(&hist_buf);
    finish_preamble(buf, 1, header_end);
}

struct Preamble {
    format: u16,
    header_len: usize,
    payload_len: u64,
    payload_crc: u32,
    header_crc: u32,
}

fn parse_preamble(b: &[u8]) -> Option<Preamble> {
    let mut c = Cur { b, p: 0 };
    if c.arr::<8>()? != MAGIC {
        return None;
    }
    let format = c.u16()?;
    if !(1..=FORMAT_VERSION).contains(&format) {
        return None;
    }
    Some(Preamble {
        format,
        header_len: c.u16()? as usize,
        payload_len: c.u64()?,
        payload_crc: c.u32()?,
        header_crc: c.u32()?,
    })
}

/// Reads and checks only the preamble and header (startup index scan). `Ok(None)` for an invalid
/// or truncated file; the payload CRC is checked on [`decode`].
pub(crate) fn read_header(path: &Path) -> io::Result<Option<(Header, u64)>> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut pre = [0u8; PREAMBLE_LEN];
    if len < PREAMBLE_LEN as u64 || file.read_exact(&mut pre).is_err() {
        return Ok(None);
    }
    let Some(p) = parse_preamble(&pre) else {
        return Ok(None);
    };
    if (PREAMBLE_LEN + p.header_len) as u64 + p.payload_len != len {
        return Ok(None);
    }
    let mut hb = vec![0u8; p.header_len];
    if file.read_exact(&mut hb).is_err() || crc32(&hb) != p.header_crc {
        return Ok(None);
    }
    Ok(decode_header(&mut Cur { b: &hb, p: 0 }, p.format).map(|h| (h, len)))
}

/// Reads a whole tile. `Ok(None)` for an invalid, truncated or corrupt file.
pub(crate) fn decode(path: &Path, g: &LevelGeometry, bins: usize) -> io::Result<Option<Tile>> {
    let bytes = fs::read(path)?;
    Ok(decode_bytes(&bytes, g, bins))
}

pub(crate) fn decode_bytes(bytes: &[u8], g: &LevelGeometry, bins: usize) -> Option<Tile> {
    let p = parse_preamble(bytes)?;
    let header_end = PREAMBLE_LEN + p.header_len;
    if header_end as u64 + p.payload_len != bytes.len() as u64 {
        return None;
    }
    let (hb, stored) = (&bytes[PREAMBLE_LEN..header_end], &bytes[header_end..]);
    if crc32(hb) != p.header_crc || crc32(stored) != p.payload_crc {
        return None;
    }
    let h = decode_header(&mut Cur { b: hb, p: 0 }, p.format)?;
    if h.nt as usize != g.nt || usize::from(h.hist.bins) != bins {
        return None;
    }
    let inflated;
    let payload = match h.codec {
        PayloadCodec::Raw => stored,
        PayloadCodec::Zstd => {
            if h.raw_payload_len > MAX_RAW_PAYLOAD {
                return None;
            }
            inflated = zstd::bulk::decompress(stored, h.raw_payload_len as usize).ok()?;
            if inflated.len() as u64 != h.raw_payload_len {
                return None;
            }
            &inflated[..]
        }
    };
    let key = TileKey {
        scheme: h.scheme,
        level: h.level,
        f_block: h.f_block,
        t_block: h.t_block,
    };
    let nf = h.nf as usize;
    let mut tile = Tile::new(key, nf, g, bins);
    tile.prov = h.prov;
    let n = nf * g.nt;
    let mut c = Cur { b: payload, p: 0 };
    let bitmap = c.take(n.div_ceil(8))?;
    let observed = |i: usize| bitmap[i / 8] & (1 << (i % 8)) != 0;
    if h.format == 1 {
        for i in (0..n).filter(|&i| observed(i)) {
            let max = dq_db(i16::from_le_bytes(c.arr()?));
            let mean = dq_db(i16::from_le_bytes(c.arr()?));
            let plo = dq_db(i16::from_le_bytes(c.arr()?));
            let phi = dq_db(i16::from_le_bytes(c.arr()?));
            let occ = dq_frac(c.u16()?);
            let occ_max = dq_frac(c.u16()?);
            let cov = dq_frac(c.u16()?);
            let count = u32::try_from(c.varint()?).ok()?;
            tile.set_decoded(i, count, max, mean, plo, phi, occ, occ_max, cov);
        }
    } else {
        let m = (0..n).filter(|&i| observed(i)).count();
        let cols: Vec<&[u8]> = (0..N_COLS)
            .map(|_| c.take(m.checked_mul(2)?))
            .collect::<Option<_>>()?;
        let at = |col: usize, j: usize| [cols[col][2 * j], cols[col][2 * j + 1]];
        for (j, i) in (0..n).filter(|&i| observed(i)).enumerate() {
            let count = u32::try_from(c.varint()?).ok()?;
            tile.set_decoded(
                i,
                count,
                dq_db(i16::from_le_bytes(at(0, j))),
                dq_db(i16::from_le_bytes(at(1, j))),
                dq_db(i16::from_le_bytes(at(2, j))),
                dq_db(i16::from_le_bytes(at(3, j))),
                dq_frac(u16::from_le_bytes(at(4, j))),
                dq_frac(u16::from_le_bytes(at(5, j))),
                dq_frac(u16::from_le_bytes(at(6, j))),
            );
        }
    }
    let hbitmap = c.take(nf.div_ceil(8))?;
    for f in 0..nf {
        if hbitmap[f / 8] & (1 << (f % 8)) == 0 {
            continue;
        }
        let first = c.varint()? as usize;
        let len = c.varint()? as usize;
        if first.checked_add(len)? > bins {
            return None;
        }
        for b in first..first + len {
            tile.hist[f * bins + b] = u32::try_from(c.varint()?).ok()?;
        }
    }
    if c.p != payload.len() {
        return None;
    }
    if key.level == 0 {
        tile.col_done = (0..g.nt)
            .rev()
            .find(|&t| (0..nf).any(|f| tile.count[t * nf + f] > 0));
        // T-584: every row this tile carries is folded upwards by the reopen rebuild (see
        // `Pyramid::open`), so live maintenance must not fold them a second time.
        tile.folded_through = tile.col_done;
    }
    Some(tile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_reference_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn varint_round_trip() {
        let mut b = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            b.clear();
            put_varint(&mut b, v);
            assert_eq!(Cur { b: &b, p: 0 }.varint(), Some(v));
        }
    }

    /// T-1024: a tile round-trips every plane it stores, and re-encoding the decoded tile is a
    /// fixed point (quantisation happens once). The measurement harness rests on both.
    #[test]
    fn every_plane_round_trips_and_re_encodes_identically() {
        let g = LevelGeometry {
            f_cell_hz: 1000.0,
            t_cell_ns: 1_000_000_000,
            nt: 8,
            f_factor: 1,
            t_factor: 1,
            from: None,
        };
        let hist = HistogramConfig {
            lo_db: -200.0,
            step_db: 5.0,
            bins: 44,
        };
        let key = TileKey {
            scheme: 9,
            level: 0,
            f_block: 0,
            t_block: 0,
        };
        let bins = usize::from(hist.bins);
        let mut tile = Tile::new(key, 16, &g, bins);
        tile.t_cell_s = 1.0;
        let mut seed = 12345u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / 16_777_216.0 * 3.0
        };
        for i in 0..16 * 8 {
            if i % 5 == 4 {
                continue; // unobserved
            }
            let v = -100.0 + rnd();
            // max == mean (one frame), p_lo a hair below, p_hi exactly max, coverage constant.
            tile.set_decoded(i, 1, v, v, v - 0.01, v, 0.5, 0.5, 1.0);
        }
        for fmt in [2u16, FORMAT_VERSION] {
            let (mut buf, mut scratch) = (Vec::new(), Vec::new());
            encode_format(
                fmt,
                &tile,
                true,
                PowerUnit::Dbfs,
                &g,
                &hist,
                (10.0, 90.0),
                None,
                &mut buf,
                &mut scratch,
            );
            let back = decode_bytes(&buf, &g, bins).expect("decodes");
            assert_eq!(back.count, tile.count, "format {fmt}: frames");
            let close = |a: f32, b: f32, what: &str, i: usize| {
                assert!(
                    (a - b).abs() <= 0.006 || (a == b) || (a.is_nan() && b.is_nan()),
                    "format {fmt}: {what} at {i}: {a} vs {b}"
                );
            };
            for i in 0..16 * 8 {
                close(back.max[i], tile.max[i], "max", i);
                close(back.p_lo[i], tile.p_lo[i], "p_lo", i);
                close(back.p_hi[i], tile.p_hi[i], "p_hi", i);
                close(back.mean_db(i), tile.mean_db(i), "mean", i);
                close(back.occ_max[i], tile.occ_max[i], "occupancy_max", i);
                close(back.obs_s[i] as f32, tile.obs_s[i] as f32, "coverage", i);
                close(back.occ_s[i] as f32, tile.occ_s[i] as f32, "occupancy", i);
            }
            // Re-encoding the decoded tile is a fixed point: quantisation happened once.
            let mut again = Vec::new();
            encode_format(
                fmt,
                &back,
                true,
                PowerUnit::Dbfs,
                &g,
                &hist,
                (10.0, 90.0),
                None,
                &mut again,
                &mut scratch,
            );
            assert_eq!(again, buf, "format {fmt}: re-encode is byte-identical");
        }
    }

    #[test]
    fn quantisation() {
        assert_eq!(dq_db(q_db(-93.456)), -93.46);
        assert!(dq_db(q_db(f32::NAN)).is_nan());
        assert_eq!(dq_frac(q_frac(1.0)), 1.0);
    }
}
