//! T-297: a **normal run** characterises a swept region, through the mock SDR device.
//!
//! T-294 measured that `hk_dsp::chirp` recovers a sweep rate from one analysis frame of IQ, and
//! left it deliberately uncalled. That is the fifth capability-with-no-caller in waiting (T-242
//! signature matching, T-247 the C15 classifier, T-206's own gate dimensions, T-267/T-287 the CC
//! hunt were the first four), and the pattern is the finding: **a capability that nothing calls is
//! indistinguishable from an absent one**, and its tests pass either way.
//!
//! So this suite closes the vacuum from the other side, and deliberately does the opposite of
//! `hk_dsp::chirp`'s own tests: it never mentions `linear_sweep`, `chirp_rate`, a lag or a PAPR.
//! It starts a run the way `hk serve` does — on a truth-stripped recording served by the mock SDR,
//! with the **built-in** chain registry, no plan override and nothing configured by the test — and
//! then asks the repository what the run measured.
//!
//! **The falsifiable pair.** Remove the wiring (`chains::sweep`, or the `sweep-char` spec from the
//! built-in registry) and this fails. Remove the estimator and T-294's own four-species and
//! SNR-floor tests fail. Neither can pass vacuously.
//!
//! **Blind.** The scene is T-255's: spreading factor, bandwidth and payload live in annotations
//! `blind_replay` strips and seals before the device opens the recording. The run is told nothing
//! but where the device says it is tuned. Truth is opened at the end, and only to check the answer.
//!
//! **What is asserted, and what deliberately is not.** That a sweep rate reached the repository for
//! the region that sweeps, that it is the right rate, and that the three species that do *not*
//! sweep were examined and left alone. Not that the emission is LoRa: a sweep rate is evidence
//! about a region, never an identity, and nothing in the run names a protocol.

use hk_e2e::{SynthRequest, synth_or_skip};
use hk_model::signature::field;
use hk_model::{EmitterId, FreqRange, Region, Repository};

use crate::blind::{BlindSource, blind_replay};
use crate::common::*;

const T297: &str = "T-297";

/// Tolerance on the recovered sweep rate, as a fraction of truth.
///
/// A priori, with its arithmetic. T-294 recovers `α` to **1.6 %** on the exact SF9 geometry
/// (`fs` = the channel width, one 4.096 ms frame). Through the pipeline two things loosen that and
/// nothing tightens it: the region the chain channelises is the *detector's* box, which T-255
/// measures at 0.98 of the true channel, and the DDC floors to an integer decimation so the
/// realised rate is at or above the requested one. 20 % covers both with room to spare, and is
/// still eight times tighter than the SF9/SF12 rate ratio — so it cannot be passed by measuring
/// the wrong emission.
const RATE_TOLERANCE: f64 = 0.20;

/// The sweep rate this run measured for the emitter nearest `center_hz`, if it measured one.
///
/// `Ok(None)` is the answer for an emitter that exists and was **not** characterised, which is the
/// control this suite needs; `Err` means no emitter covered that region at all.
fn measured_rate(repo: &Repository, center_hz: f64, bandwidth_hz: f64) -> Result<Option<f64>, ()> {
    let emitters = repo
        .emitters_in_region(&Region::new(
            FreqRange::centered(center_hz, 4.0 * bandwidth_hz),
            ever(),
        ))
        .expect("emitters");
    let nearest: Option<EmitterId> = emitters
        .iter()
        .min_by(|a, b| {
            (a.f_center_hz - center_hz)
                .abs()
                .total_cmp(&(b.f_center_hz - center_hz).abs())
        })
        .map(|e| e.id);
    let id = nearest.ok_or(())?;
    Ok(repo
        .emitter_features(id)
        .expect("features")
        .and_then(|f| f.num(field::SWEEP_RATE_HZ_PER_S)))
}

#[test]
fn t297_a_normal_run_characterises_a_swept_region() {
    let out = synth_or_skip!(SynthRequest::new("lora_ism_burst").seed(255));
    let fx = out.fixture(0).unwrap();
    let run = blind_replay(&fx.meta_path, "t297", BlindSource::default());
    let repo = repo(&run.dir.0);

    let count = |p: &str| run.summary.counter(p);
    let (passes, characterised) = (
        count("/chains/sweep_passes"),
        count("/chains/sweep_characterised"),
    );
    let (uncharacterised, no_emitter) = (
        count("/chains/sweep_uncharacterised"),
        count("/chains/sweep_no_emitter"),
    );
    eprintln!(
        "[{T297}] characterisation during the run: {passes} pass(es), {characterised} \
         characterised, {uncharacterised} examined and left alone, {no_emitter} measured with no \
         emitter to attach to, {} attach(es) refused by the cap",
        count("/chains/sweep_admission_refused")
    );

    // ---- The wiring ran at all. This is the assertion T-294 could not make.
    assert!(
        passes >= 1,
        "[{T297}] nothing read IQ for a candidate region during a normal run: the characteriser \
         was never attached, so `hk_dsp::chirp` still has no caller"
    );
    assert_eq!(
        no_emitter, 0,
        "[{T297}] a sweep was measured but had no emitter to be evidence about"
    );

    // ---- Truth, opened only now, and only to check the answer.
    let packets = fx.of_kind("lora-packet");
    let cws = fx.of_kind("cw");
    let bursts = fx.of_kind("fsk-burst");
    assert!(
        !packets.is_empty() && !cws.is_empty() && !bursts.is_empty(),
        "[{T297}] the scene must carry the swept emission and its two fixed-frequency controls"
    );
    let bw = packets[0].expect_f64("bandwidth_hz");
    let t_sym = packets[0].expect_f64("symbol_duration_s");
    // The sweep rate of a LoRa up-chirp: one channel per symbol, i.e. BW²/2^SF.
    let truth_rate = bw / t_sym;
    let chan_center = 0.5 * (packets[0].f_lo_hz + packets[0].f_hi_hz);

    // ---- The swept region was characterised, and characterised correctly.
    let got = measured_rate(&repo, chan_center, bw)
        .expect("an emitter for the swept region")
        .unwrap_or_else(|| {
            panic!(
                "[{T297}] the run detected the swept region but claimed nothing about it: no \
                 {} on its emitter, though the estimator recovers it from one frame of the IQ",
                field::SWEEP_RATE_HZ_PER_S
            )
        });
    let err = (got - truth_rate).abs() / truth_rate;
    eprintln!(
        "[{T297}] swept region at {:.4} MHz: α measured {:.4e} Hz/s against truth {:.4e} Hz/s \
         ({:.1} % out, floor {:.0} %) — measured from the IQ, which is the only product it is in",
        chan_center / 1e6,
        got,
        truth_rate,
        100.0 * err,
        100.0 * RATE_TOLERANCE
    );
    assert!(
        err <= RATE_TOLERANCE,
        "[{T297}] the sweep rate is {got:.4e} Hz/s but the emission sweeps {truth_rate:.4e} Hz/s \
         ({:.1} % out)",
        100.0 * err
    );
    assert!(
        characterised >= 1,
        "[{T297}] a rate reached the repository without the run counting a characterisation"
    );

    // ---- The controls. A steady carrier and a 2-FSK burst are examined by the same chain and
    // must be left alone: `unknown` is "not measured", so nothing is written for them at all.
    // Without this the assertion above could be satisfied by a chain that calls everything a sweep.
    for (what, item) in [("steady carrier", cws[0]), ("2-FSK burst", bursts[0])] {
        match measured_rate(&repo, item.center_hz(), item.bandwidth_hz().max(1e3)) {
            Ok(Some(rate)) => panic!(
                "[{T297}] a {what} at {:.4} MHz was given a sweep rate of {rate:.4e} Hz/s: this \
                 characteriser calls things sweeps that do not sweep",
                item.center_hz() / 1e6
            ),
            Ok(None) => eprintln!(
                "[{T297}] control: the {what} at {:.4} MHz carries no sweep rate, as it must",
                item.center_hz() / 1e6
            ),
            Err(()) => eprintln!("[{T297}] control: no emitter for the {what}; nothing claimed"),
        }
    }
    assert!(
        uncharacterised >= 1,
        "[{T297}] no region was examined and left uncharacterised, so the controls prove nothing: \
         either they were never looked at, or everything was called a sweep"
    );
}
