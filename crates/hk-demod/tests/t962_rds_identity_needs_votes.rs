//! T-962 (SIGNAL-062): an RDS PI is an **identity** only once its vote clears the commit bound.
//!
//! **The defect.** On 2026-09-25 the explorer's FM second pass CONFIRMED 98.085 MHz as PI 1704
//! from about three CRC-valid groups in 45 s, while an independent oracle
//! (`py/fixtures/rds_ref.py`) found no RDS at all on the same station and the same clip. The app
//! claimed more than the data supported: three agreeing blocks were written straight through as a
//! transmitter identity, and an identity is what `ConfirmPolicy`'s route A confirms on — a
//! lifecycle change no rule demotes.
//!
//! **The bound, and which one.** ADR-0022 §6's `analytic_holdout_bits` budget governs
//! `ConfirmPolicy.synthesized`, the confirm route for a *synthesized* pipeline whose check stage
//! was searched and whose look-elsewhere the engine can price (§5.1–§5.2). RDS takes none of that
//! path: it is a shipped, template-fixed decoder confirming through route A, and no
//! `analytic_holdout_bits` is computed for it. The bound here is therefore the ticket's other
//! option — **N agreeing CRC-valid groups**, `GroupConfig::pi_commit_votes` = 10 — and ADR-0022
//! §1.3 is why it exists: a confirm is irreversible, so the confirm gate binds, always.
//!
//! **What this test asserts, on the data-model objects.**
//!
//! - A three-group PI writes its Decode row with `pi`, `pi_votes` and `pi_provisional: true` and
//!   **no** `Decode::identity`, and places **no** Emitter under `rds-pi:C0DE`. The reading is
//!   kept and shown; the claim is not made.
//! - The same station heard for a second commits: `Decode::identity` is set and the Emitter
//!   carries the `rds-pi` identity, exactly as before.
//!
//! The evidence is built at the bit level, where "two or three chance CRC passes" is defined — a
//! front end in front of it would add noise to the question, not evidence about it.

mod common;

use common::*;
use hk_demod::rds::{GroupConfig, Offset, RDS_BITRATE_BD, RdsDecoder, RdsReport, encode_block};
use hk_demod::{AnalogMode, AnalogReceiver, AnalogSession, RecordContext, write_session};
use hk_estimate::SnippetRequest;
use hk_model::{DecodedIdentity, IdentityScheme, Repository};
use num_complex::Complex32;

const T962: &str = "T-962";
const FS: f64 = 500e3;
/// Bits in one RDS group: 4 blocks of 26.
const GROUP_BITS: usize = 104;

/// Bits of a 0A group carrying PS segment `seg` of `ps` under `pi`.
fn group_0a_bits(pi: u16, ps: &[u8; 8], seg: usize) -> Vec<u8> {
    let b2 = (1u16 << 10) | (10u16 << 5) | seg as u16;
    let b4 = (u16::from(ps[2 * seg]) << 8) | u16::from(ps[2 * seg + 1]);
    [
        encode_block(pi, Offset::A),
        encode_block(b2, Offset::B),
        encode_block(0xE0CD, Offset::C),
        encode_block(b4, Offset::D),
    ]
    .iter()
    .flat_map(|b| (0..26).map(move |i| ((b >> (25 - i)) & 1) as u8))
    .collect()
}

/// The decoder's report after `groups` clean 0A groups of PI `C0DE` / PS `HACKRIFF`.
fn report_over(groups: usize) -> RdsReport {
    let mut dec = RdsDecoder::new(GroupConfig::default(), RDS_BITRATE_BD);
    let mut bits = Vec::with_capacity(groups * GROUP_BITS);
    for g in 0..groups {
        bits.extend(group_0a_bits(0xC0DE, b"HACKRIFF", g % 4));
    }
    for (i, &b) in bits.iter().enumerate() {
        dec.push_bit(b, i as f64);
    }
    dec.report()
}

/// A real WFM session over a synthetic broadcast scene, with `rds` as what its RDS chain read.
///
/// The session comes from the receiver so every other field is a measurement; only the RDS report
/// — the thing under test — is placed, at exactly the evidence level being asserted about.
fn session_with(rds: RdsReport) -> AnalogSession {
    let iq: Vec<Complex32> = nbfm_tone(FS, 1.0, 40e3, 0.1, 75e3, 4e3);
    let prov = provenance(100e6, FS);
    let req = SnippetRequest {
        start_index: 0,
        end_index: iq.len() as u64,
        center_offset_hz: 40e3,
        bandwidth_hz: 200e3,
    };
    let mut s = AnalogReceiver::default()
        .run(info(0, &prov), &iq, &req)
        .expect("[T-962] the receiver runs on a broadcast-width FM scene");
    assert_eq!(s.mode.mode, AnalogMode::Wfm, "[{T962}] {:?}", s.mode);
    let wfm = s
        .wfm
        .as_mut()
        .expect("[T-962] a WFM decision runs the WFM chain");
    wfm.rds = Some(rds);
    s
}

fn pi_c0de() -> DecodedIdentity {
    DecodedIdentity {
        scheme: IdentityScheme::RdsPi,
        value: "C0DE".into(),
    }
}

#[test]
fn t962_a_three_group_pi_is_recorded_and_is_not_an_identity() {
    // Five groups: sync acquires on three consecutive offset-consistent blocks, so the first two
    // groups pay for the lock and three CRC-valid PI blocks are left — the 98.085 MHz evidence.
    let rds = report_over(5);
    let pi = rds
        .pi
        .expect("[T-962] the reading is kept, not abstained away");
    assert_eq!(pi.votes, 3, "[{T962}] {rds:?}");
    assert!(pi.provisional, "[{T962}] {pi:?}");

    let s = session_with(rds);
    let mut repo = Repository::open_in_memory().unwrap();
    let w = write_session(&mut repo, &s, &RecordContext::default()).unwrap();

    assert!(
        repo.emitter_by_identity(&pi_c0de()).unwrap().is_none(),
        "[{T962}] three agreeing CRC-valid groups placed an Emitter under rds-pi:C0DE. That row \
         is what ConfirmPolicy's route A confirms on, and a confirm is irreversible (ADR-0022 \
         §1.3): the identity may not be written until the vote clears pi_commit_votes."
    );
    assert!(
        w.emitter_id.is_none() && !w.emitter_created,
        "[{T962}] no caller hint and no committed identity: nothing to attach to. {w:?}"
    );

    // The reading is still recorded, with the vote count the UI needs to say "3 groups,
    // provisional".
    let rows: Vec<_> = w
        .decode_ids
        .iter()
        .map(|d| repo.decode(*d).unwrap())
        .collect();
    let row = rows
        .iter()
        .find(|d| d.frame_model == "rds-pi")
        .expect("[T-962] a refusal is a record too: the rds-pi row is written either way");
    assert_eq!(row.metadata["pi"], "C0DE", "[{T962}] {:?}", row.metadata);
    assert_eq!(row.metadata["pi_votes"], 3, "[{T962}] {:?}", row.metadata);
    assert_eq!(
        row.metadata["pi_provisional"], true,
        "[{T962}] the row must say the PI is provisional, or a reader cannot tell a three-group \
         reading from a committed one: {:?}",
        row.metadata
    );
    assert!(
        rows.iter().all(|d| d.identity.is_none()),
        "[{T962}] a provisional PI carries no DecodedIdentity on any row: {:?}",
        rows.iter().map(|d| &d.identity).collect::<Vec<_>>()
    );
    assert!(
        repo.decodes_for_identity(&pi_c0de()).unwrap().is_empty(),
        "[{T962}] and so none of them is identity evidence for the confirm gate"
    );
}

#[test]
fn t962_a_second_of_a_real_station_commits_the_identity() {
    // 12 groups ~ 1.05 s at 11.4 groups/s: what the bound actually costs a real station.
    let rds = report_over(12);
    let pi = rds.pi.expect("[T-962] PI");
    assert!(
        pi.votes >= GroupConfig::default().pi_commit_votes && pi.committed(),
        "[{T962}] a second of clean air must clear the bound, or the bound is not a bound but a \
         ban: {pi:?}"
    );

    let s = session_with(rds);
    let mut repo = Repository::open_in_memory().unwrap();
    let w = write_session(&mut repo, &s, &RecordContext::default()).unwrap();

    let e = repo
        .emitter_by_identity(&pi_c0de())
        .unwrap()
        .expect("[T-962] a committed PI still places its Emitter");
    assert_eq!(Some(e.id), w.emitter_id);
    assert!(w.emitter_created);
    let rows: Vec<_> = w
        .decode_ids
        .iter()
        .map(|d| repo.decode(*d).unwrap())
        .collect();
    let row = rows.iter().find(|d| d.frame_model == "rds-pi").unwrap();
    assert_eq!(row.identity.as_ref(), Some(&pi_c0de()), "[{T962}]");
    assert_eq!(
        row.metadata["pi_provisional"], false,
        "[{T962}] {:?}",
        row.metadata
    );
    assert!(
        !repo.decodes_for_identity(&pi_c0de()).unwrap().is_empty(),
        "[{T962}] the identity evidence route A reads is back"
    );
}
