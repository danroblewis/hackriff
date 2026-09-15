//! The tile file format. Little-endian throughout. Version 2 (T-116) is written; version 1
//! (T-017) is still read, so no migration is needed: v1 tiles stay valid until evicted.
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
//! payload (v1)     observed bitmap (nt·nf bits, row-major t then f)
//!                  per observed cell: max i16 · mean i16 · p_low i16 · p_high i16 (0.01 dB,
//!                    i16::MIN = unknown) · occupancy u16 · occupancy_max u16 · coverage u16
//!                    (fractions × 65535) · frames varint
//!                  histogram bitmap (nf bits); per present row: first bin varint · len varint ·
//!                    len counts varint (leading/trailing zero bins trimmed)
//! payload (v2)     after zstd decompression when codec = 1: observed bitmap · then one column per
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

use hk_model::{CalibrationStateId, PowerUnit, SpurMaskId, TileKey, Timestamp};

use super::config::{HistogramConfig, LevelGeometry};
use super::frame::{FrontEnd, GainState, PortTag};
use super::tile::{
    FrontEndState, MAX_GAIN_STATES, MAX_PROVENANCE_STEPS, ProvenanceStep, ProvenanceSummary, Tile,
};

const MAGIC: [u8; 8] = *b"HKTILE\0\x01";
/// Tile file format version written (T-116: 2). Version 1 is still read.
pub const FORMAT_VERSION: u16 = 2;
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

fn q_db(v: f32) -> i16 {
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

fn dq_db(v: i16) -> f32 {
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

fn put_state(buf: &mut Vec<u8>, s: &FrontEndState) {
    let g = s.gain.unwrap_or_default();
    buf.push(u8::from(s.gain.is_some()));
    buf.extend_from_slice(&g.lna_db.to_le_bytes());
    buf.extend_from_slice(&g.vga_db.to_le_bytes());
    buf.push(u8::from(g.amp_on));
    put_uuid_text(buf, s.calibration);
    put_opt_u32(buf, s.front_end.gain_table);
    put_opt_tag(buf, s.front_end.filter);
    put_uuid_text(buf, s.front_end.spur_mask);
}

fn get_state(c: &mut Cur<'_>) -> Option<FrontEndState> {
    let present = c.u8()? != 0;
    let g = GainState {
        lna_db: c.f32()?,
        vga_db: c.f32()?,
        amp_on: c.u8()? != 0,
    };
    Some(FrontEndState {
        gain: present.then_some(g),
        calibration: c.uuid_text::<CalibrationStateId>()?,
        front_end: FrontEnd {
            gain_table: c.opt_u32()?,
            filter: c.opt_tag()?,
            spur_mask: c.uuid_text::<SpurMaskId>()?,
        },
    })
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
        put_state(buf, &s.from);
        put_state(buf, &s.to);
    }
    buf.push(match h.codec {
        PayloadCodec::Raw => 0,
        PayloadCodec::Zstd => 1,
    });
    buf.extend_from_slice(&h.raw_payload_len.to_le_bytes());
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
            let from = get_state(c)?;
            let to = get_state(c)?;
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

/// The v2 raw payload: bitmap, columns, histograms.
fn encode_payload(tile: &Tile, buf: &mut Vec<u8>) {
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
    let cells = || (0..n).filter(|&i| tile.count[i] > 0);
    for i in cells() {
        buf.extend_from_slice(&q_db(tile.max[i]).to_le_bytes());
    }
    for i in cells() {
        buf.extend_from_slice(&q_db(tile.mean_db(i)).to_le_bytes());
    }
    for i in cells() {
        buf.extend_from_slice(&q_db(tile.p_lo[i]).to_le_bytes());
    }
    for i in cells() {
        buf.extend_from_slice(&q_db(tile.p_hi[i]).to_le_bytes());
    }
    for i in cells() {
        let (obs, occ) = tile.cell_obs(i);
        let ratio = if obs > 0.0 { occ / obs } else { 0.0 };
        buf.extend_from_slice(&q_frac(ratio).to_le_bytes());
    }
    for i in cells() {
        buf.extend_from_slice(&q_frac(f64::from(tile.occ_max[i])).to_le_bytes());
    }
    for i in cells() {
        let (obs, _) = tile.cell_obs(i);
        buf.extend_from_slice(&q_frac(obs / tile.t_cell_s).to_le_bytes());
    }
    for i in cells() {
        put_varint(buf, u64::from(tile.count[i]));
    }
    encode_histograms(tile, buf);
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
        format: FORMAT_VERSION,
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
    finish_preamble(buf, FORMAT_VERSION, header_end);
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
        let cols: Vec<&[u8]> = (0..7)
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

    #[test]
    fn quantisation() {
        assert_eq!(dq_db(q_db(-93.456)), -93.46);
        assert!(dq_db(q_db(f32::NAN)).is_nan());
        assert_eq!(dq_frac(q_frac(1.0)), 1.0);
    }
}
