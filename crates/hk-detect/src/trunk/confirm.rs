//! Control-channel confirmation (C23): frame sync **and** valid CRC, never occupancy alone.
//!
//! # Why this module is shaped the way it is
//!
//! C23's "Pitfalls" names the failure directly: *"False CCs: continuous data emitters pass the
//! FCO test. Require sync plus CRC."* A control channel is described as continuous 4FSK/C4FM at
//! 100 % frequency-channel occupancy on the LMR raster — but that description is satisfied by
//! any continuous data emitter parked on a 12.5 kHz grid. Occupancy and raster are **candidacy**.
//! Confirmation is framing.
//!
//! So the split is structural, not a convention a later caller can forget:
//!
//! - [`CcCandidate`] is what FCO and a raster fit can produce. It is freely constructible and
//!   carries **no** method that yields a confirmation, a protocol, or a `TrunkSystem`.
//! - [`CcEvidence`] records that sync and CRC were both observed. Its fields are private and it
//!   has **no public constructor**; the only value of this type anywhere in the workspace comes
//!   out of [`CcConfirmer::confirm`].
//! - [`ConfirmedCc`] holds a `CcEvidence` in a private field, so it cannot be built by struct
//!   literal from any other module.
//!
//! Nothing here derives `serde::Deserialize` — indeed nothing here touches serde at all — so
//! there is no deserialisation path that fabricates evidence either. T-266 had to defend its
//! `Encryption` type against exactly that; the cheapest defence is to have no such path.
//!
//! # Scope
//!
//! Confirming a control channel is not the same as naming its protocol. [`ConfirmedCc::protocol`]
//! reports [`TrunkProtocol::Unknown`]: T-267 establishes *that* a CC is there, T-268 decodes
//! TSBKs and says *what* it is. The matched pattern is kept in the evidence so T-268 has a
//! starting point without this module overclaiming.

use hk_estimate::framing::crc::BitCrc;
use hk_model::TrunkProtocol;

use super::dmr::{CSBK_BYTES, DMR_BS_DATA_SYNC_DIBITS, csbk_crc};
use super::raster::RasterFit;

/// Dibits (4FSK symbols) in the P25 Phase 1 frame sync `0x5575F5FF77FF` (docs/04 §7.3).
///
/// 48 bits at 2 bits per C4FM symbol. The pattern uses only the outer symbols, which is a real
/// property of P25 and one that makes the noise floor below conservative rather than optimistic:
/// a 4-level slicer on noise puts more mass on the inner levels than on the outer ones, so a
/// random symbol matches an all-outer pattern *less* often than a uniform model predicts.
pub const P25_FRAME_SYNC_DIBITS: [u8; 24] = [
    1, 1, 1, 1, 1, 3, 1, 1, 3, 3, 1, 1, 3, 3, 3, 3, 1, 3, 1, 3, 3, 3, 3, 3,
];

/// Data + CRC bytes in the block that follows the sync.
pub const BLOCK_BYTES: usize = 12;
/// Dibits in that block (4 dibits per byte).
pub const BLOCK_DIBITS: usize = BLOCK_BYTES * 4;
/// Dibits in one sync-plus-block frame.
pub const FRAME_DIBITS: usize = P25_FRAME_SYNC_DIBITS.len() + BLOCK_DIBITS;

// ---------------------------------------------------------------------------------------------
// A-priori thresholds.
//
// These were fixed, with the arithmetic below, BEFORE the detector was run on anything. Never
// edit one of these to make a run pass: tuning a threshold against an acceptance result is a
// protocol violation, not a judgement call (ADR-0016 §7, "A-priori thresholds"; the same rule
// m3_grid.rs states for its classification floors). A floor changes only with a documented
// justification of its own, never because a measurement came out on the wrong side of it.
// ---------------------------------------------------------------------------------------------

/// Symbol mismatches tolerated across the 24-dibit frame sync.
///
/// A priori: on pure noise a dibit matches by chance with probability 1/4, so the exact binomial
/// probability of a correlation hit at one trial position is
/// `Σ_{k≤T} C(24,k)·3^k / 4^24` — 3.6e-15 at T=0, 2.6e-13 at T=1, **9.1e-12 at T=2**, 2.0e-10 at
/// T=3. T=2 buys roughly an 8 % symbol-error budget on a real control channel (a signal with a
/// 2 % symbol error rate still clears 24 symbols with ≥ 98 % probability) while keeping the
/// chance rate eleven orders of magnitude below one-in-a-billion per trial.
pub const SYNC_TOLERANCE_DIBITS: u32 = 2;

/// Frame syncs that must be found before a candidate may be confirmed.
///
/// A priori: a control channel is *continuous*, so over any useful dwell it emits many frames.
/// Requiring more than one collapses the already-negligible chance rate by squaring it and costs
/// a real CC nothing.
pub const MIN_SYNC_HITS: u32 = 2;

/// CRC-valid blocks that must be found before a candidate may be confirmed.
///
/// A priori: a 16-bit CRC passes random data with probability 2^-16 = 1.5e-5. Combined with the
/// sync gate the per-trial chance of a confirmable frame is 9.1e-12 × 1.5e-5 = **1.4e-16**; at
/// 4800 symbols/s that is one expected false confirmation per ~1.5e12 channel-seconds, i.e.
/// never. Two is cheap for a continuous emitter and impossible for noise.
pub const MIN_CRC_VALID: u32 = 2;

/// Occupancy below which a channel is not even a candidate.
///
/// A priori: control channels transmit continuously — docs/04 §4 records "control channels
/// transmit continuously: 100 % duty cycle". 0.95 leaves room for measurement gaps without
/// admitting bursty traffic. This is a **candidacy** floor: clearing it proves nothing, and
/// lowering it cannot cause a false confirmation, only extra work.
pub const MIN_CC_FCO: f64 = 0.95;

/// The floor the measured sync false-alarm rate on pure noise is asserted against, per trial.
///
/// A priori: the exact chance rate at [`SYNC_TOLERANCE_DIBITS`] is 9.1e-12 (above). 1e-5 sits
/// ~1.1e6× above that, which is deliberate slack for the one way reality departs from the model:
/// recovered symbols on noise are not independent, because the channel filter and the
/// integrate-and-dump correlate adjacent ones. It is also above the resolution a finite test can
/// honestly claim — with 2e6 trials and zero hits the 95 % upper bound is only 1.5e-6 — so the
/// assertion promises exactly what it can measure. A rate above 1e-5 would mean the correlator
/// is broken, not merely non-ideal.
pub const SYNC_FALSE_ALARM_FLOOR: f64 = 1e-5;

/// A control channel may never be confirmed from noise. This is the operational claim; the
/// arithmetic above puts the expectation at ~1e-16 per trial, so any nonzero count is a bug.
pub const CONFIRMED_FALSE_ALARM_MAX: u64 = 0;

// ---------------------------------------------------------------------------------------------
// Framings (T-271)
// ---------------------------------------------------------------------------------------------

/// A control-channel framing this build can confirm: a frame sync, a block length, and the CRC that
/// block carries.
///
/// T-267 kept the matched pattern in [`CcEvidence::pattern`] precisely so a later task could add a
/// second one. This is that task, and the shape it takes matters: **a framing is an air interface,
/// not a trunking protocol.** A DMR base-station data burst is emitted by a conventional Tier II
/// repeater and by a trunked Tier III control channel alike, so matching one says "DMR", never
/// "trunked DMR". Naming a *protocol* stays where T-268 put it — behind corroborated control
/// messages — and this enum deliberately has no method that yields a [`TrunkProtocol`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CcFraming {
    /// P25 Phase 1: the 48-bit frame sync `0x5575F5FF77FF` and a 12-byte block (T-267).
    P25Phase1,
    /// DMR base-station **data** burst: the 48-bit sync `0xDFF57D75DF5D` and a 12-byte CSBK whose
    /// CRC-CCITT is masked with `0xA5A5` (T-271). Simplified in the ways
    /// [`super::dmr`] lists — no BPTC, no interleaving, the burst flattened.
    DmrBsData,
}

/// Every framing this build looks for, in the order it tries them.
///
/// Order is not a priority: the gates are so far below chance (9.1e-12 per sync trial) that two
/// framings cannot both match the same stream by accident, and the first hit wins only because
/// something has to.
pub const CC_FRAMINGS: [CcFraming; 2] = [CcFraming::P25Phase1, CcFraming::DmrBsData];

impl CcFraming {
    /// The name recorded in [`CcEvidence::pattern`].
    pub const fn name(self) -> &'static str {
        match self {
            Self::P25Phase1 => "p25-frame-sync",
            Self::DmrBsData => "dmr-bs-data-sync",
        }
    }

    /// The sync dibits to correlate against.
    pub const fn sync_dibits(self) -> &'static [u8] {
        match self {
            Self::P25Phase1 => &P25_FRAME_SYNC_DIBITS,
            Self::DmrBsData => &DMR_BS_DATA_SYNC_DIBITS,
        }
    }

    /// Bytes in the block that follows the sync.
    pub const fn block_bytes(self) -> usize {
        match self {
            Self::P25Phase1 => BLOCK_BYTES,
            Self::DmrBsData => CSBK_BYTES,
        }
    }

    /// Dibits in one sync-plus-block frame.
    pub const fn frame_dibits(self) -> usize {
        self.sync_dibits().len() + self.block_bytes() * 4
    }
}

/// Settings for [`CcConfirmer`]. `Default` is the a-priori set above.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CcConfirmConfig {
    /// Symbol mismatches tolerated across the frame sync.
    pub sync_tolerance: u32,
    /// Frame syncs required to confirm.
    pub min_sync_hits: u32,
    /// CRC-valid blocks required to confirm.
    pub min_crc_valid: u32,
}

impl Default for CcConfirmConfig {
    fn default() -> Self {
        Self {
            sync_tolerance: SYNC_TOLERANCE_DIBITS,
            min_sync_hits: MIN_SYNC_HITS,
            min_crc_valid: MIN_CRC_VALID,
        }
    }
}

/// A channel that occupancy and a raster fit make **worth examining**.
///
/// Constructing one asserts nothing about what the emitter is. There is deliberately no
/// `into_confirmed`, no `protocol()`, and no path from here to a `TrunkSystem`: the only way
/// onward is [`CcConfirmer::confirm`], which needs demodulated symbols this type does not have.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CcCandidate {
    /// Measured centre, Hz (the measurement, never snapped to the grid).
    pub center_hz: f64,
    /// Measured bandwidth, Hz.
    pub bandwidth_hz: f64,
    /// Frequency-channel occupancy over the observation, 0–1.
    pub fco: f64,
    /// The grid the centre lands on.
    pub raster: RasterFit,
}

impl CcCandidate {
    /// A candidate, if the channel is occupied enough to be one.
    ///
    /// Returns `None` below [`MIN_CC_FCO`]: bursty traffic is not a control channel. Returning
    /// `Some` means only "continuous, and on a raster" — the two things C23 warns are not enough.
    pub fn new(center_hz: f64, bandwidth_hz: f64, fco: f64, raster: RasterFit) -> Option<Self> {
        (center_hz.is_finite()
            && bandwidth_hz.is_finite()
            && bandwidth_hz > 0.0
            && fco.is_finite()
            && fco >= MIN_CC_FCO)
            .then_some(Self {
                center_hz,
                bandwidth_hz,
                fco,
                raster,
            })
    }
}

/// Proof that a candidate carried framing: frame syncs **and** CRC-valid blocks.
///
/// Fields are private and there is no public constructor. The only way to obtain one is
/// [`CcConfirmer::confirm`] returning `Some`, which happens only when both gates are met. That
/// is what stops FCO alone from promoting a candidate anywhere in the workspace.
#[derive(Clone, Debug, PartialEq)]
pub struct CcEvidence {
    sync_hits: u32,
    crc_checked: u32,
    crc_valid: u32,
    pattern: &'static str,
    trials: u64,
}

impl CcEvidence {
    /// Frame syncs found.
    pub fn sync_hits(&self) -> u32 {
        self.sync_hits
    }
    /// Blocks whose CRC was checked (one per sync hit).
    pub fn crc_checked(&self) -> u32 {
        self.crc_checked
    }
    /// Blocks whose CRC was valid.
    pub fn crc_valid(&self) -> u32 {
        self.crc_valid
    }
    /// The sync pattern that matched. T-268 maps this to a protocol.
    pub fn pattern(&self) -> &'static str {
        self.pattern
    }
    /// Symbol positions examined.
    pub fn trials(&self) -> u64 {
        self.trials
    }
}

/// A candidate that framing confirmed.
///
/// The `evidence` field is private, so this struct cannot be built by literal outside this
/// module; combined with [`CcEvidence`]'s missing constructor, a `ConfirmedCc` can only come
/// from [`CcConfirmer::confirm`].
#[derive(Clone, Debug, PartialEq)]
pub struct ConfirmedCc {
    candidate: CcCandidate,
    framing: CcFraming,
    evidence: CcEvidence,
}

impl ConfirmedCc {
    /// The candidacy this was confirmed from.
    pub fn candidate(&self) -> &CcCandidate {
        &self.candidate
    }
    /// The framing that matched — i.e. which **air interface** this is, which is not the same
    /// statement as which trunking protocol it speaks. See [`CcFraming`].
    pub fn framing(&self) -> CcFraming {
        self.framing
    }
    /// The framing evidence that confirmed it.
    pub fn evidence(&self) -> &CcEvidence {
        &self.evidence
    }
    /// The control-channel frequency, Hz — the **measured** centre, not the grid centre.
    ///
    /// `TrunkSystem::cc_freq_hz` is `Option<f64>` where `None` means *no dedicated control
    /// channel* (Capacity Plus, LTR) and the column admits only `NULL` or `> 0.0` (T-266). A
    /// confirmed CC always has a real frequency, so this returns `f64`, and a caller writing a
    /// `TrunkSystem` wraps it in `Some`.
    pub fn cc_freq_hz(&self) -> f64 {
        self.candidate.center_hz
    }
    /// Always [`TrunkProtocol::Unknown`]: confirming a control channel precedes naming its
    /// protocol, which is T-268's job. `Unknown` exists in the T-266 model for exactly this gap.
    pub fn protocol(&self) -> TrunkProtocol {
        TrunkProtocol::Unknown
    }
}

/// What a scan of a symbol stream found. Reported whether or not it confirms, so a rejection is
/// as legible as a confirmation — and so the false-alarm rate on noise can be measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScanOutcome {
    /// Symbol positions examined.
    pub trials: u64,
    /// Positions whose sync correlation was within tolerance.
    pub sync_hits: u32,
    /// Blocks whose CRC was checked.
    pub crc_checked: u32,
    /// Blocks whose CRC was valid.
    pub crc_valid: u32,
}

impl ScanOutcome {
    /// Sync hits per trial — the quantity [`SYNC_FALSE_ALARM_FLOOR`] bounds on noise.
    pub fn sync_false_alarm_rate(&self) -> f64 {
        if self.trials == 0 {
            0.0
        } else {
            f64::from(self.sync_hits) / self.trials as f64
        }
    }
}

/// Finds frame sync and checks CRCs in a demodulated 4FSK symbol stream.
///
/// Input is dibits (values 0–3), so this crate needs no DSP: symbol recovery belongs to the
/// demodulator, and keeping it out here is what lets the false-alarm floor be measured over
/// millions of synthetic symbols in a unit test.
#[derive(Clone, Debug)]
pub struct CcConfirmer {
    cfg: CcConfirmConfig,
    crc: BitCrc,
    dmr_crc: BitCrc,
}

impl Default for CcConfirmer {
    fn default() -> Self {
        Self::new(CcConfirmConfig::default())
    }
}

impl CcConfirmer {
    /// A confirmer with `cfg`.
    pub fn new(cfg: CcConfirmConfig) -> Self {
        // CRC-16/CCITT-FALSE: poly 0x1021, init 0xFFFF, no reflection, xorout 0. Computing it
        // across data *and* the appended CRC leaves 0, which is the check used below.
        let crc = BitCrc::new(16, 0x1021, 0xFFFF, false, false, 0)
            .expect("CRC-16/CCITT-FALSE is a valid BitCrc");
        Self {
            cfg,
            crc,
            // Held rather than rebuilt per block: a `BitCrc` carries a 256-entry table.
            dmr_crc: csbk_crc(),
        }
    }

    /// Settings.
    pub fn config(&self) -> &CcConfirmConfig {
        &self.cfg
    }

    /// Whether `block` carries a valid CRC for `framing`.
    ///
    /// The two framings differ in more than a constant, which is why this is a match rather than a
    /// parameter: P25's check is "the CRC over the data **and** the appended CRC comes to zero";
    /// DMR's is "the CRC over the first ten bytes, masked with 0xA5A5, equals the stored two". One
    /// shared "check the CRC" would have had to pick one and be silently wrong for the other.
    fn crc_ok(&self, framing: CcFraming, block: &[u8]) -> bool {
        match framing {
            CcFraming::P25Phase1 => self.crc.compute(block, 0, BLOCK_BYTES * 8) == 0,
            CcFraming::DmrBsData => {
                block.len() >= CSBK_BYTES && {
                    let want = (u16::from(block[10]) << 8) | u16::from(block[11]);
                    self.dmr_crc.compute(&block[..10], 0, 80) as u16 == want
                }
            }
        }
    }

    /// Scans `dibits` for P25 frame syncs and CRC-valid blocks (T-267's signature, unchanged).
    pub fn scan(&self, dibits: &[u8]) -> ScanOutcome {
        self.scan_framing(CcFraming::P25Phase1, dibits)
    }

    /// Scans `dibits` for `framing`'s frame syncs and CRC-valid blocks, reporting what it found.
    ///
    /// Every position that can hold a whole frame is a trial, so `trials` is the denominator of
    /// the false-alarm rate.
    pub fn scan_framing(&self, framing: CcFraming, dibits: &[u8]) -> ScanOutcome {
        let (sync, frame) = (framing.sync_dibits(), framing.frame_dibits());
        let block_dibits = framing.block_bytes() * 4;
        let mut out = ScanOutcome::default();
        if dibits.len() < frame {
            return out;
        }
        let last = dibits.len() - frame;
        for i in 0..=last {
            out.trials += 1;
            if !self.sync_matches(sync, dibits, i) {
                continue;
            }
            out.sync_hits += 1;
            let start = i + sync.len();
            let bytes = pack_dibits(&dibits[start..start + block_dibits]);
            out.crc_checked += 1;
            if self.crc_ok(framing, &bytes) {
                out.crc_valid += 1;
            }
        }
        out
    }

    /// Whether `sync` matches `dibits` at `i` within the tolerance.
    fn sync_matches(&self, sync: &[u8], dibits: &[u8], i: usize) -> bool {
        let mut miss = 0u32;
        for (k, &want) in sync.iter().enumerate() {
            if dibits[i + k] != want {
                miss += 1;
                if miss > self.cfg.sync_tolerance {
                    return false;
                }
            }
        }
        true
    }

    /// The CRC-valid P25 blocks in `dibits`, sync-aligned, for [`super::tsbk`] to decode (T-268).
    pub fn crc_valid_blocks(&self, dibits: &[u8]) -> Vec<[u8; BLOCK_BYTES]> {
        self.crc_valid_blocks_framing(CcFraming::P25Phase1, dibits)
            .into_iter()
            .map(|b| {
                let mut out = [0u8; BLOCK_BYTES];
                out.copy_from_slice(&b);
                out
            })
            .collect()
    }

    /// The CRC-valid blocks in `dibits` for `framing`, sync-aligned, for a protocol decoder
    /// ([`super::tsbk`] for P25, [`super::dmr`] for DMR).
    ///
    /// This yields **bytes, not evidence**. A block is bytes that passed a CRC behind a frame
    /// sync; it is not a [`CcEvidence`] and cannot become one, so the type-level rule T-267
    /// established still holds: [`Self::confirm_framing`] remains the only source of a
    /// [`ConfirmedCc`] anywhere in the workspace. Decoding what a control channel *said* is a
    /// separate question from whether it is one, and the caller has to have answered the second
    /// question first.
    pub fn crc_valid_blocks_framing(&self, framing: CcFraming, dibits: &[u8]) -> Vec<Vec<u8>> {
        let (sync, frame) = (framing.sync_dibits(), framing.frame_dibits());
        let block_dibits = framing.block_bytes() * 4;
        let mut out = Vec::new();
        if dibits.len() < frame {
            return out;
        }
        let last = dibits.len() - frame;
        let mut i = 0;
        while i <= last {
            if !self.sync_matches(sync, dibits, i) {
                i += 1;
                continue;
            }
            let start = i + sync.len();
            let bytes = pack_dibits(&dibits[start..start + block_dibits]);
            if self.crc_ok(framing, &bytes) {
                out.push(bytes);
                // A frame that checked out is a frame: resume after it rather than re-examining
                // every symbol inside it, so one frame cannot yield two overlapping "blocks".
                i = start + block_dibits;
                continue;
            }
            i += 1;
        }
        out
    }

    /// Confirms `candidate` from its demodulated symbols under P25 framing (T-267's signature).
    pub fn confirm(&self, candidate: &CcCandidate, dibits: &[u8]) -> Option<ConfirmedCc> {
        self.confirm_framing(CcFraming::P25Phase1, candidate, dibits)
    }

    /// Confirms `candidate` from its demodulated symbols under `framing`, or returns `None`.
    ///
    /// `Some` requires **both** gates: at least `min_sync_hits` frame syncs and at least
    /// `min_crc_valid` CRC-valid blocks. Occupancy and raster fit are already baked into the
    /// existence of the `CcCandidate` and are deliberately not re-examined here — they cannot
    /// substitute for framing, which is the whole point.
    pub fn confirm_framing(
        &self,
        framing: CcFraming,
        candidate: &CcCandidate,
        dibits: &[u8],
    ) -> Option<ConfirmedCc> {
        let s = self.scan_framing(framing, dibits);
        (s.sync_hits >= self.cfg.min_sync_hits && s.crc_valid >= self.cfg.min_crc_valid).then_some(
            ConfirmedCc {
                candidate: *candidate,
                framing,
                evidence: CcEvidence {
                    sync_hits: s.sync_hits,
                    crc_checked: s.crc_checked,
                    crc_valid: s.crc_valid,
                    pattern: framing.name(),
                    trials: s.trials,
                },
            },
        )
    }

    /// Confirms `candidate` under **any** framing this build knows, returning the first that both
    /// gates admit (T-271).
    ///
    /// Trying more framings cannot make a false confirmation likely: each carries its own
    /// independent 1.4e-16-per-frame chance rate (see [`MIN_CRC_VALID`]), so two of them is 2.8e-16,
    /// and the union bound stays astronomically below one per device lifetime. What it does cost is
    /// one more correlation pass per candidate, which is why [`CC_FRAMINGS`] is a short fixed list
    /// rather than an open registry.
    pub fn confirm_any(&self, candidate: &CcCandidate, dibits: &[u8]) -> Option<ConfirmedCc> {
        CC_FRAMINGS
            .iter()
            .find_map(|&f| self.confirm_framing(f, candidate, dibits))
    }
}

/// Packs MSB-first dibits into bytes (4 dibits per byte).
fn pack_dibits(dibits: &[u8]) -> Vec<u8> {
    dibits
        .chunks(4)
        .map(|c| c.iter().fold(0u8, |acc, &d| (acc << 2) | (d & 3)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trunk::raster::{RASTER_TOLERANCE_HZ, best_lmr_raster};

    const ORIGIN: f64 = 851_012_500.0;

    /// A deterministic uniform symbol source. `hk-detect` has no RNG dependency and this needs
    /// no statistical quality beyond uniformity over 0..=3, which SplitMix64 gives comfortably.
    struct SplitMix(u64);
    impl SplitMix {
        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn dibits(&mut self, n: usize) -> Vec<u8> {
            let mut v = Vec::with_capacity(n);
            while v.len() < n {
                let mut x = self.next_u64();
                for _ in 0..32 {
                    if v.len() == n {
                        break;
                    }
                    v.push((x & 3) as u8);
                    x >>= 2;
                }
            }
            v
        }
    }

    fn crc16(data: &[u8]) -> u16 {
        let mut crc = 0xFFFFu16;
        for &b in data {
            crc ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    fn unpack_dibits(bytes: &[u8]) -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|&b| [(b >> 6) & 3, (b >> 4) & 3, (b >> 2) & 3, b & 3])
            .collect()
    }

    /// `n` back-to-back frames: P25 sync + 10 data bytes + their CRC.
    fn cc_stream(seed: u64, n: usize) -> Vec<u8> {
        let mut rng = SplitMix(seed);
        let mut out = Vec::new();
        for _ in 0..n {
            out.extend_from_slice(&P25_FRAME_SYNC_DIBITS);
            let data: Vec<u8> = (0..10).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let mut block = data.clone();
            block.extend_from_slice(&crc16(&data).to_be_bytes());
            out.extend_from_slice(&unpack_dibits(&block));
        }
        out
    }

    fn candidate() -> CcCandidate {
        let f = ORIGIN + 3.0 * 12_500.0;
        CcCandidate::new(
            f,
            12_500.0,
            1.0,
            best_lmr_raster(f, ORIGIN, RASTER_TOLERANCE_HZ).unwrap(),
        )
        .expect("continuous and on-raster")
    }

    #[test]
    fn the_local_crc_reference_matches_the_repo_implementation() {
        // Guards the test's own oracle: CRC-16/CCITT-FALSE of "123456789" is 0x29B1.
        assert_eq!(crc16(b"123456789"), 0x29B1);
        let c = CcConfirmer::default();
        let mut block = b"123456789".to_vec();
        block.extend_from_slice(&crc16(b"123456789").to_be_bytes());
        assert_eq!(c.crc.compute(&block, 0, block.len() * 8), 0);
    }

    #[test]
    fn a_real_control_channel_is_confirmed() {
        let c = CcConfirmer::default();
        let got = c
            .confirm(&candidate(), &cc_stream(7, 8))
            .expect("sync and CRC both present");
        assert!(got.evidence().sync_hits() >= MIN_SYNC_HITS);
        assert!(got.evidence().crc_valid() >= MIN_CRC_VALID);
        assert_eq!(got.evidence().pattern(), "p25-frame-sync");
        // Confirming a CC does not name its protocol; T-268 does that.
        assert_eq!(got.protocol(), TrunkProtocol::Unknown);
        // The measured centre is reported, not the grid centre.
        assert_eq!(got.cc_freq_hz(), candidate().center_hz);
    }

    /// C23's named pitfall, as a test: the decoy is continuous, on-raster and 4FSK, so it is a
    /// perfectly good candidate — and it must still be rejected.
    #[test]
    fn a_continuous_data_emitter_is_a_candidate_and_is_still_rejected() {
        let cand = candidate();
        assert_eq!(cand.fco, 1.0, "100 % occupancy: passes the FCO test");
        assert_eq!(cand.raster.spacing_hz, 12_500.0, "on the LMR raster");

        let c = CcConfirmer::default();
        let decoy = SplitMix(99).dibits(200_000);
        let s = c.scan(&decoy);
        assert!(s.trials > 0);
        assert!(
            c.confirm(&cand, &decoy).is_none(),
            "continuous unframed 4FSK must never confirm: scan {s:?}"
        );
    }

    /// The gates are independent: neither alone promotes.
    #[test]
    fn sync_without_valid_crc_does_not_confirm() {
        let mut rng = SplitMix(5);
        let mut stream = Vec::new();
        for _ in 0..8 {
            stream.extend_from_slice(&P25_FRAME_SYNC_DIBITS);
            // A block with a deliberately wrong CRC: sync is perfect, framing is not.
            let data: Vec<u8> = (0..10).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let mut block = data.clone();
            block.extend_from_slice(&(crc16(&data) ^ 0xFFFF).to_be_bytes());
            stream.extend_from_slice(&unpack_dibits(&block));
        }
        let c = CcConfirmer::default();
        let s = c.scan(&stream);
        assert!(s.sync_hits >= MIN_SYNC_HITS, "syncs are found: {s:?}");
        assert_eq!(s.crc_valid, 0, "but no CRC is valid: {s:?}");
        assert!(c.confirm(&candidate(), &stream).is_none());
    }

    #[test]
    fn bursty_occupancy_is_not_even_a_candidate() {
        let f = ORIGIN + 12_500.0;
        let raster = best_lmr_raster(f, ORIGIN, RASTER_TOLERANCE_HZ).unwrap();
        assert!(CcCandidate::new(f, 12_500.0, 0.4, raster).is_none());
        assert!(CcCandidate::new(f, 12_500.0, MIN_CC_FCO, raster).is_some());
    }

    /// The a-priori floor, measured. Thresholds are fixed in the consts above with their
    /// arithmetic; this run may not move them.
    #[test]
    fn sync_false_alarm_rate_on_pure_noise_is_below_the_a_priori_floor() {
        let c = CcConfirmer::default();
        let noise = SplitMix(0xC0FFEE).dibits(2_000_000);
        let s = c.scan(&noise);
        let rate = s.sync_false_alarm_rate();
        eprintln!(
            "[T-267] pure-noise scan: trials {} sync_hits {} crc_valid {} rate {:.3e} \
             (a-priori floor {:.0e}, exact chance rate 9.1e-12)",
            s.trials, s.sync_hits, s.crc_valid, rate, SYNC_FALSE_ALARM_FLOOR
        );
        assert!(
            s.trials >= 1_900_000,
            "enough trials to mean something: {s:?}"
        );
        assert!(
            rate <= SYNC_FALSE_ALARM_FLOOR,
            "sync false-alarm rate {rate:.3e} exceeds the a-priori floor {SYNC_FALSE_ALARM_FLOOR:.0e}"
        );
        assert_eq!(
            u64::from(s.crc_valid),
            CONFIRMED_FALSE_ALARM_MAX,
            "noise produced a CRC-valid block: {s:?}"
        );
        let cand = candidate();
        assert!(c.confirm(&cand, &noise).is_none(), "noise confirmed a CC");
    }

    /// `n` back-to-back DMR frames: the BS-data sync + a 12-byte CSBK with its masked CRC.
    fn dmr_stream(seed: u64, n: usize) -> Vec<u8> {
        let mut rng = SplitMix(seed);
        let crc = crate::trunk::dmr::csbk_crc();
        let mut out = Vec::new();
        for _ in 0..n {
            out.extend_from_slice(&crate::trunk::dmr::DMR_BS_DATA_SYNC_DIBITS);
            let mut block: Vec<u8> = (0..10).map(|_| (rng.next_u64() & 0xFF) as u8).collect();
            let c = crc.compute(&block, 0, 80) as u16;
            block.extend_from_slice(&c.to_be_bytes());
            out.extend_from_slice(&unpack_dibits(&block));
        }
        out
    }

    /// The second framing, confirmed the same way the first is — and the evidence says **which**
    /// air interface matched, because that is the starting point a protocol decoder needs (T-271).
    #[test]
    fn a_dmr_control_channel_is_confirmed_and_names_its_own_framing() {
        let c = CcConfirmer::default();
        let got = c
            .confirm_any(&candidate(), &dmr_stream(11, 8))
            .expect("DMR sync and masked CRC both present");
        assert_eq!(got.framing(), CcFraming::DmrBsData);
        assert_eq!(got.evidence().pattern(), "dmr-bs-data-sync");
        assert!(got.evidence().crc_valid() >= MIN_CRC_VALID);
        // Confirming an air interface still does not name a trunking protocol: a conventional
        // Tier II repeater emits the same bursts, so only decoded CSBKs may say "trunked".
        assert_eq!(got.protocol(), TrunkProtocol::Unknown);
    }

    /// The framings do not bleed into each other: each stream confirms under its own and **only**
    /// its own. Without this, `confirm_any` could pass by matching whatever it tried first.
    #[test]
    fn neither_framing_confirms_the_other_ones_control_channel() {
        let c = CcConfirmer::default();
        let cand = candidate();
        let p25 = cc_stream(7, 8);
        let dmr = dmr_stream(11, 8);

        assert!(
            c.confirm_framing(CcFraming::P25Phase1, &cand, &p25)
                .is_some()
        );
        assert!(
            c.confirm_framing(CcFraming::DmrBsData, &cand, &p25)
                .is_none(),
            "a P25 control channel confirmed as DMR"
        );
        assert!(
            c.confirm_framing(CcFraming::DmrBsData, &cand, &dmr)
                .is_some()
        );
        assert!(
            c.confirm_framing(CcFraming::P25Phase1, &cand, &dmr)
                .is_none(),
            "a DMR control channel confirmed as P25"
        );
        // `confirm_any` picks the right one for each rather than the first in the list.
        assert_eq!(
            c.confirm_any(&cand, &p25).unwrap().framing(),
            CcFraming::P25Phase1
        );
        assert_eq!(
            c.confirm_any(&cand, &dmr).unwrap().framing(),
            CcFraming::DmrBsData
        );
        // And the blocks handed to a protocol decoder are that framing's, not the other's.
        assert!(
            !c.crc_valid_blocks_framing(CcFraming::DmrBsData, &dmr)
                .is_empty()
        );
        assert!(
            c.crc_valid_blocks_framing(CcFraming::DmrBsData, &p25)
                .is_empty()
        );
    }

    /// Adding a framing may not add a way to confirm noise. The same pure-noise stream is scanned
    /// under **every** framing, and none of them may find a CRC-valid block.
    #[test]
    fn no_framing_confirms_pure_noise() {
        let c = CcConfirmer::default();
        let noise = SplitMix(0xBADC0FFEE).dibits(1_000_000);
        for f in CC_FRAMINGS {
            let s = c.scan_framing(f, &noise);
            eprintln!(
                "[T-271] {} on noise: trials {} sync_hits {} crc_valid {} (rate {:.3e})",
                f.name(),
                s.trials,
                s.sync_hits,
                s.crc_valid,
                s.sync_false_alarm_rate()
            );
            assert!(
                s.sync_false_alarm_rate() <= SYNC_FALSE_ALARM_FLOOR,
                "{} sync false-alarm rate exceeds the a-priori floor",
                f.name()
            );
            assert_eq!(
                u64::from(s.crc_valid),
                CONFIRMED_FALSE_ALARM_MAX,
                "{} found a CRC-valid block in noise",
                f.name()
            );
        }
        assert!(c.confirm_any(&candidate(), &noise).is_none());
    }

    #[test]
    fn a_short_stream_yields_no_trials_rather_than_a_panic() {
        let c = CcConfirmer::default();
        assert_eq!(c.scan(&[]), ScanOutcome::default());
        assert_eq!(c.scan(&[1, 1, 1]).trials, 0);
        assert_eq!(c.scan(&[1u8; FRAME_DIBITS - 1]).trials, 0);
        assert_eq!(c.scan(&[1u8; FRAME_DIBITS]).trials, 1);
    }
}
