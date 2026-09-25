//! Building conventional DMR bursts, for the tests and synthetic scenes that feed [`super::scan`].
//!
//! This is the **encoder half** of the decoder above, written so the two meet only at the air
//! interface: a burst is assembled field by field from the layout in the module docs, coded with
//! [`super::fec`], and handed back as dibits a 4FSK modulator can transmit. A test that builds a
//! burst here and reads it back with [`super::scan`] exercises the framing, the interleave, both
//! Hamming codes, the Golay slot type and the header checks end to end.
//!
//! It is not a DMR transmitter: there is no vocoder, no slot timing against a real clock, no
//! superframe state machine and no reverse channel. What it produces is what a receiver has to
//! cope with — a downlink of 30 ms slots, each a CACH and a burst.

use super::{
    BURST_DIBITS, CACH_BITS, CACH_DIBITS, DataType, INFO_DIBITS, SLOT_GRID_DIBITS,
    SLOT_TYPE_HALF_DIBITS, SYNC_DIBITS, cach_encode, fec, slot_type_encode,
};

/// Packs bits (0/1, MSB first) into dibits.
fn dibits_of(bits: &[u8]) -> Vec<u8> {
    debug_assert_eq!(bits.len() % 2, 0, "a dibit is two bits");
    bits.chunks(2).map(|c| (c[0] << 1) | (c[1] & 1)).collect()
}

/// A deterministic filler for bits this build does not model (vocoder frames, embedded
/// signalling). An LCG rather than a constant, so a filler can never masquerade as a sync word.
pub struct Filler(u64);

impl Filler {
    /// A filler seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// `n` pseudo-random bits.
    pub fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n)
            .map(|_| {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (self.0 >> 61 & 1) as u8
            })
            .collect()
    }
}

/// A data burst: 196 BPTC-coded information bits, the slot type either side of the sync, and the
/// sync itself. 132 dibits.
pub fn data_burst(
    sync_index: usize,
    colour_code: u8,
    data_type: DataType,
    payload: &[u8; fec::BPTC_PAYLOAD_BYTES],
) -> Vec<u8> {
    let raw = fec::bptc_196_96_encode(payload);
    let st = slot_type_encode(colour_code, data_type);
    let mut bits = Vec::with_capacity(BURST_DIBITS * 2);
    bits.extend_from_slice(&raw[..INFO_DIBITS * 2]);
    bits.extend_from_slice(&st[..SLOT_TYPE_HALF_DIBITS * 2]);
    let sync = super::DMR_SYNCS[sync_index];
    for i in 0..SYNC_DIBITS {
        bits.push((sync.hex >> (47 - 2 * i) & 1) as u8);
        bits.push((sync.hex >> (46 - 2 * i) & 1) as u8);
    }
    bits.extend_from_slice(&st[SLOT_TYPE_HALF_DIBITS * 2..]);
    bits.extend_from_slice(&raw[INFO_DIBITS * 2..]);
    let out = dibits_of(&bits);
    debug_assert_eq!(out.len(), BURST_DIBITS);
    out
}

/// A voice burst: 216 filler bits around a 48-bit middle that is either the voice sync (burst A of
/// a superframe) or embedded signalling this build does not model. 132 dibits.
pub fn voice_burst(sync_index: Option<usize>, filler: &mut Filler) -> Vec<u8> {
    let mut bits = filler.bits(108);
    match sync_index {
        Some(i) => {
            let sync = super::DMR_SYNCS[i];
            for b in 0..48 {
                bits.push((sync.hex >> (47 - b) & 1) as u8);
            }
        }
        None => bits.extend(filler.bits(48)),
    }
    bits.extend(filler.bits(108));
    let out = dibits_of(&bits);
    debug_assert_eq!(out.len(), BURST_DIBITS);
    out
}

/// One downlink slot: a CACH then a burst, 144 dibits = 30 ms.
pub fn slot(tdma_channel: u8, lcss: u8, burst: &[u8], filler: &mut Filler) -> Vec<u8> {
    let mut short_lc = [0u8; 17];
    for (i, b) in filler.bits(17).into_iter().enumerate() {
        short_lc[i] = b;
    }
    let cach = cach_encode(1, tdma_channel, lcss, &short_lc);
    debug_assert_eq!(cach.len(), CACH_BITS);
    let mut out = dibits_of(&cach);
    debug_assert_eq!(out.len(), CACH_DIBITS);
    out.extend_from_slice(burst);
    debug_assert_eq!(out.len(), SLOT_GRID_DIBITS);
    out
}

/// A voice LC header block, ready for [`data_burst`].
pub fn voice_lc_header(flco: u8, destination: u32, source: u32) -> [u8; fec::BPTC_PAYLOAD_BYTES] {
    super::full_lc_encode(
        &super::FullLc {
            protect: false,
            flco,
            fid: 0,
            service_options: 0,
            destination,
            source,
        },
        super::VOICE_LC_HEADER_MASK,
    )
}

/// A terminator-with-LC block.
pub fn terminator_with_lc(
    flco: u8,
    destination: u32,
    source: u32,
) -> [u8; fec::BPTC_PAYLOAD_BYTES] {
    super::full_lc_encode(
        &super::FullLc {
            protect: false,
            flco,
            fid: 0,
            service_options: 0,
            destination,
            source,
        },
        super::TERMINATOR_LC_MASK,
    )
}

/// A packet-data header block.
pub fn data_header(
    group: bool,
    destination: u32,
    source: u32,
    blocks_to_follow: u8,
) -> [u8; fec::BPTC_PAYLOAD_BYTES] {
    super::data_header_encode(&super::DataHeader {
        group,
        response_requested: false,
        dpf: 0,
        sap: 0,
        destination,
        source,
        blocks_to_follow,
    })
}

/// A conventional repeater's downlink over `slots` 30 ms slots: short calls in slot 1, idle
/// bursts in slot 2, as dibits.
///
/// The call's addresses are `(destination, source)`; `colour_code` is the repeater's. The call
/// cycle is **four bursts** — voice LC header, data header, a voice superframe burst carrying the
/// voice sync, terminator with LC — repeated for as long as the downlink runs. That is a busier
/// repeater than a real one (a real call's voice runs for seconds between its header and its
/// terminator), and it is deliberate: a receiver analyses a *window*, so a scene whose headers all
/// sit in its first tenth of a second tests only whether the window happened to land there. Here
/// every four-burst window of the call slot carries one of each, so what is measured is whether
/// the headers decode, not where the analyser looked.
pub fn repeater_downlink(slots: usize, colour_code: u8, destination: u32, source: u32) -> Vec<u8> {
    let mut filler = Filler::new(0x5EED_D317);
    let mut out = Vec::with_capacity(slots * SLOT_GRID_DIBITS);
    let idle = [0u8; fec::BPTC_PAYLOAD_BYTES];
    for i in 0..slots {
        // One TDMA frame is two slots: the call is in slot 1, slot 2 is idle.
        let call = i % 2 == 0;
        let n = i / 2;
        let burst = if !call {
            data_burst(1, colour_code, DataType::Idle, &idle)
        } else {
            match n % 4 {
                0 => data_burst(
                    1,
                    colour_code,
                    DataType::VoiceLcHeader,
                    &voice_lc_header(0, destination, source),
                ),
                1 => data_burst(
                    1,
                    colour_code,
                    DataType::DataHeader,
                    &data_header(true, destination, source, 2),
                ),
                // Burst A of a voice superframe carries the voice sync; B-E carry embedded
                // signalling this build does not model, and are filler.
                2 => voice_burst(Some(0), &mut filler),
                _ => data_burst(
                    1,
                    colour_code,
                    DataType::TerminatorWithLc,
                    &terminator_with_lc(0, destination, source),
                ),
            }
        };
        out.extend(slot(u8::from(call), 0, &burst, &mut filler));
    }
    out
}
