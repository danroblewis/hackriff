//! Band-plan-derived `hk-mod@1` family priors (ADR-0016 §3, T-212).
//!
//! [`BandPlanFamilyPriors`] implements `hk_model::classify::FamilyPriors`: given an emission's
//! measured extent, it looks up the overlapping rows of the C17 [`BandTable`] and turns their
//! allocation into `P(family ∣ f, ℓ)` over the `hk-mod@1` taxonomy — the prior `fuse()` mixes with
//! the classifier's likelihood. It never classifies and never vetoes a measurement: with no
//! overlapping row, or with rows whose use has no reliable single-family judgement (amateur bands,
//! notably), it returns `None`, and `fuse()` then leaves the posterior equal to the likelihood
//! (ADR-0016 §3 "No C17 data").
//!
//! **Provenance and judgement.** [`row_family_weights`] is the only place a human guess enters:
//! most rows (`fm-broadcast`, `aero-radionav-960-1215`, the GNSS bands, `noaa-*`) have one
//! essentially-definitional modulation and get weight 1.0. A few genuinely mixed bands (the ISM
//! bands, marine VHF, 700/800 MHz public safety) get a small, explicitly-commented split. Nothing
//! here is measured; each split cites the reasoning in its comment so an explanation can say *why*
//! a prior applied, and every row not listed abstains rather than guesses. This is a coarse,
//! allocation-only prior (`λ = [λ₀, λ₁, 0, 0]`, no licence extract or per-emitter history yet); the
//! λ₂/λ₃ slots ADR-0016 reserves for those stay zero until a later task fills them in.

use std::collections::BTreeMap;

use hk_model::classify::{FamilyPriorSet, FamilyPriors, LabelP};

use crate::band_table::{BandTable, LoadError, Region};

/// Uniform-mixture weight λ₀ this source always uses: the ADR floor (`LAMBDA0_MIN` in
/// `hk_model::classify`), kept at its minimum because allocation is the only evidence behind this
/// prior (no licence extract, no per-emitter history yet — the λ₂/λ₃ slots).
const LAMBDA0: f64 = 0.1;

type FamilyWeights = &'static [(&'static str, f64)];

/// Judgement-call mapping from an allocation-table row id (`hk-context/data/us-47cfr2106-compact.csv`)
/// to the `hk-mod@1` family (or families) its occupants are expected to use, weighted by rough
/// share. None of these splits are measured; they encode ordinary-course knowledge of what each
/// band actually carries, same as `known_status::expected_tags` does for the coarser known/unknown
/// call. A row not listed here (every `amateur-*` row, plus anything the CSV adds later) yields no
/// prior at all rather than a guess: amateur allocations carry CW, SSB, FM voice and a long tail of
/// digital modes with no dominant one, so a forced split would be noise, not signal — the fail-safe
/// CLAUDE.md and the C17 card require ("no record" must never read as "safe to assume").
fn row_family_weights(id: &str) -> Option<FamilyWeights> {
    match id {
        // Broadcast FM is, definitionally, wideband FM.
        "fm-broadcast" => Some(&[("analog", 1.0)]),
        // VOR/ILS and aviation voice are both amplitude-modulated.
        "aviation-vor-ils" | "aviation-vhf-comm" => Some(&[("analog", 1.0)]),
        // The NOAA-15/18/19 APT downlink and the NOAA Weather Radio broadcast are both analog FM.
        "noaa-apt-137" | "noaa-wx" => Some(&[("analog", 1.0)]),
        // Marine VHF carries analog FM voice channels and AIS (GMSK, the `fsk` family) side by
        // side; judgement split favouring voice, the more numerous channel use on this band.
        "maritime-vhf" => Some(&[("analog", 0.6), ("fsk", 0.4)]),
        // 902-928 MHz US ISM (the "915 MHz" band): predominantly FSK/GFSK short-range devices,
        // with OOK remotes and a LoRa (chirp spread-spectrum) minority.
        "ism-902" => Some(&[("fsk", 0.6), ("ook-ask", 0.25), ("css", 0.15)]),
        // 433 MHz Part-15/SRD band: OOK (remotes, sensors) and FSK devices in roughly equal
        // numbers, with no clear majority either way.
        "ism-433-part15" => Some(&[("ook-ask", 0.5), ("fsk", 0.5)]),
        // 2.4 GHz ISM: Wi-Fi (OFDM) and Bluetooth/BLE (GFSK) split the band.
        "ism-2400" => Some(&[("ofdm", 0.5), ("fsk", 0.5)]),
        // 5.8 GHz ISM is overwhelmingly Wi-Fi/OFDM.
        "ism-5800" => Some(&[("ofdm", 1.0)]),
        // ADS-B/Mode S, DME and TACAN replies are all pulsed.
        "aero-radionav-960-1215" => Some(&[("pulsed", 1.0)]),
        // GNSS ranging codes are BPSK/QPSK spread spectrum.
        "gnss-l5" | "gnss-l2" | "gnss-l1" => Some(&[("psk-qam", 1.0)]),
        // Commercial cellular bands: assume the now-dominant LTE/OFDM air interface over legacy
        // CDMA/GSM.
        "cellular-700-ul" | "cellular-700-dl" | "cellular-850-ul" | "cellular-850-dl"
        | "aws-ul" | "aws-dl" | "pcs-ul" | "pcs-dl" => Some(&[("ofdm", 1.0)]),
        // Public-safety/SMR 700/800 MHz: P25/DMR (4-level FSK, the `fsk` family) alongside legacy
        // analog trunked FM, weighted toward the digital systems now mandated for public safety.
        "public-safety-700-a"
        | "public-safety-700-b"
        | "public-safety-800-a"
        | "public-safety-800-b" => Some(&[("fsk", 0.7), ("analog", 0.3)]),
        _ => None,
    }
}

/// A C17 [`FamilyPriors`] source derived from a [`BandTable`] (T-212).
#[derive(Clone, Debug)]
pub struct BandPlanFamilyPriors {
    table: BandTable,
}

impl BandPlanFamilyPriors {
    /// Wraps an already-loaded [`BandTable`].
    pub fn new(table: BandTable) -> Self {
        Self { table }
    }

    /// The bundled table for `region` (see [`BandTable::bundled`]).
    pub fn bundled(region: Region) -> Result<Self, LoadError> {
        Ok(Self::new(BandTable::bundled(region)?))
    }

    /// The wrapped table.
    pub fn table(&self) -> &BandTable {
        &self.table
    }
}

impl FamilyPriors for BandPlanFamilyPriors {
    fn prior_for(&self, f_lo_hz: f64, f_hi_hz: f64) -> Option<FamilyPriorSet> {
        let rows = self.table.overlapping(f_lo_hz, f_hi_hz);

        // Every overlapping row with a judgement mapping contributes one equally-weighted vote
        // (each row's own weights already sum to 1); rows with no mapping (amateur bands, or a
        // future row nobody has judged yet) simply don't vote. This is itself a small judgement —
        // treating a wide co-primary allocation the same as a narrow secondary one — cheap to
        // revisit once real mismatches show it matters.
        let mut ids: Vec<&str> = Vec::new();
        let mut combined: BTreeMap<&'static str, f64> = BTreeMap::new();
        for row in &rows {
            let Some(weights) = row_family_weights(&row.id) else {
                continue;
            };
            ids.push(row.id.as_str());
            for (family, w) in weights {
                *combined.entry(family).or_insert(0.0) += w;
            }
        }
        if combined.is_empty() {
            return None;
        }

        let n = ids.len() as f64;
        let mut dist: Vec<LabelP> = combined
            .into_iter()
            .map(|(family, w)| LabelP {
                label: family.to_owned(),
                p: w / n,
            })
            .collect();
        // Renormalise defensively: with every per-row table summing to 1.0, `dist` already sums to
        // 1 up to float error, but this keeps `FamilyPriorSet::validate` robust to a future
        // `row_family_weights` entry that doesn't.
        let sum: f64 = dist.iter().map(|lp| lp.p).sum();
        if sum > 0.0 {
            for lp in &mut dist {
                lp.p /= sum;
            }
        }

        ids.sort_unstable();
        ids.dedup();
        Some(FamilyPriorSet {
            prior_ref: format!("band-plan-family@1:{}", ids.join("+")),
            lambda: [LAMBDA0, 1.0 - LAMBDA0, 0.0, 0.0],
            dist,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hk_model::classify::{ClassFlag, fuse};

    fn priors() -> BandPlanFamilyPriors {
        BandPlanFamilyPriors::bundled(Region::Us).expect("bundled table loads")
    }

    fn lp(label: &str, p: f64) -> LabelP {
        LabelP {
            label: label.into(),
            p,
        }
    }

    fn p_of(dist: &[LabelP], label: &str) -> f64 {
        dist.iter().find(|x| x.label == label).map_or(0.0, |x| x.p)
    }

    /// The top **known** label (excludes `unknown`), matching `fuse()`'s own definition of "the
    /// evidence/posterior top" (ADR-0016 §3): a family call can win this even while `unknown`
    /// carries more mass overall, because "which family, if any" and "how confident are we it's a
    /// known family at all" are separate questions.
    fn top_label(dist: &[LabelP]) -> &str {
        dist.iter()
            .filter(|x| x.label != "unknown")
            .max_by(|a, b| a.p.total_cmp(&b.p))
            .map(|x| x.label.as_str())
            .unwrap()
    }

    /// AWARE-053: the FM broadcast allocation yields a pure `analog` prior, and the ADS-B
    /// allocation a pure `pulsed` prior (ADR-0016 §3's own examples).
    #[test]
    fn fm_broadcast_and_adsb_priors_match_the_allocation() {
        let src = priors();
        let fm = src
            .prior_for(98.0e6, 98.2e6)
            .expect("fm broadcast has a prior");
        assert_eq!(top_label(&fm.dist), "analog");
        assert!(
            (p_of(&fm.dist, "analog") - 1.0).abs() < 1e-9,
            "{:?}",
            fm.dist
        );
        assert!(fm.validate().is_ok(), "{fm:?}");

        let adsb = src
            .prior_for(1089.5e6, 1090.5e6)
            .expect("adsb extent has a prior");
        assert_eq!(top_label(&adsb.dist), "pulsed");
        assert!(adsb.validate().is_ok(), "{adsb:?}");
    }

    /// A frequency inside an amateur-only band (no judgement mapping) and one outside every
    /// bundled row both carry no prior: `fuse()` then leaves the posterior equal to the
    /// likelihood, never inventing a call from the database (CLAUDE.md, "never a source of
    /// truth").
    #[test]
    fn unmapped_and_uncovered_extents_carry_no_prior() {
        let src = priors();
        assert!(
            src.prior_for(146.0e6, 146.1e6).is_none(),
            "amateur 2m band has no dominant-family judgement"
        );
        assert!(
            src.prior_for(300.0e6, 300.1e6).is_none(),
            "no allocation row covers 300 MHz in the compact table"
        );
    }

    /// **Blind case 1: evidence dominance.** An emission sitting in the ADS-B/Mode-S allocation
    /// (prior: `pulsed` 1.0) whose evidence overwhelmingly says `fsk` (10:1 over the runner-up,
    /// e.g. an out-of-place FSK transmitter camping in that band) keeps `fsk` on top: the prior
    /// cannot override a dominant measurement, only flag the mismatch. The scenario, and which
    /// family the evidence favours, is fixed before looking at what the prior says.
    #[test]
    fn blind_evidence_dominance_is_not_flipped_by_the_prior() {
        let src = priors();
        let prior = src
            .prior_for(1089.5e6, 1090.5e6)
            .expect("adsb extent has a prior");
        assert_eq!(top_label(&prior.dist), "pulsed");

        // Evidence: fsk 0.60 vs. pulsed 0.06 (10:1) vs. unknown 0.34, decided independently of the
        // prior above.
        let likelihood = vec![lp("fsk", 0.60), lp("pulsed", 0.06), lp("unknown", 0.34)];
        let fused = fuse(&likelihood, 0.34, Some(&prior));

        assert_eq!(
            top_label(&fused.posterior),
            "fsk",
            "a dominant evidence call must survive a strongly disagreeing prior: {:?}",
            fused.posterior
        );
        assert!(
            fused.flags.contains(&ClassFlag::PriorMismatch),
            "the disagreement must still be flagged: {:?}",
            fused.flags
        );
        assert!(
            p_of(&fused.posterior, "pulsed") > 0.0,
            "no family is ever zeroed"
        );
    }

    /// **Blind case 2: a genuine tie.** An emission at 915 MHz (ISM prior: `fsk` 0.6 / `ook-ask`
    /// 0.25 / `css` 0.15) whose evidence cannot separate `ook-ask` from `css` — both measured at
    /// exactly the same likelihood — lets the prior decide in favour of the family the band
    /// actually favours between the two. Evidence alone (no prior) would break the tie
    /// alphabetically in `fuse()`, picking `css`; the prior instead moves it to `ook-ask`, which is
    /// what "reorders only within a tie" looks like when it actually fires. The tie (which two
    /// families, what likelihood) is fixed before consulting the prior.
    #[test]
    fn blind_genuine_tie_is_decided_by_the_prior() {
        let src = priors();
        let prior = src
            .prior_for(914.5e6, 915.5e6)
            .expect("915 MHz ISM has a prior");
        assert!(
            p_of(&prior.dist, "ook-ask") > p_of(&prior.dist, "css"),
            "{:?}",
            prior.dist
        );

        // Evidence: ook-ask and css exactly tied, most of the mass on the known families. Without
        // a prior, fuse()'s own tie-break picks the alphabetically-first label, "css".
        let likelihood = vec![lp("ook-ask", 0.35), lp("css", 0.35), lp("unknown", 0.30)];
        let no_prior = fuse(&likelihood, 0.30, None);
        assert_eq!(
            top_label(&no_prior.posterior),
            "css",
            "sanity: the unprimed tie-break"
        );

        let fused = fuse(&likelihood, 0.30, Some(&prior));
        assert_eq!(
            top_label(&fused.posterior),
            "ook-ask",
            "a genuine tie must go to the family the prior favours: {:?}",
            fused.posterior
        );
        assert!(
            fused.flags.contains(&ClassFlag::PriorTiebreak),
            "a tie the prior decided must be a tiebreak, not silent: {:?}",
            fused.flags
        );
        assert!(
            !fused.flags.contains(&ClassFlag::PriorMismatch),
            "a tie is not a mismatch: {:?}",
            fused.flags
        );
        assert!(
            (p_of(&fused.posterior, "unknown") - 0.30).abs() < 1e-9,
            "the prior must never move the unknown mass"
        );
    }

    /// Every mapped row's weights are themselves a valid distribution (defends the table against a
    /// future typo), and every derived prior over the bundled table validates.
    #[test]
    fn every_mapped_row_and_every_derived_prior_is_valid() {
        let table = BandTable::bundled(Region::Us).unwrap();
        for row in table.rows() {
            if let Some(weights) = row_family_weights(&row.id) {
                let sum: f64 = weights.iter().map(|(_, w)| w).sum();
                assert!((sum - 1.0).abs() < 1e-9, "{}: weights sum to {sum}", row.id);
                for (family, w) in weights {
                    assert!(
                        (0.0..=1.0).contains(w),
                        "{}: {family} weight {w} out of range",
                        row.id
                    );
                }
            }
        }

        let src = priors();
        let mut any = false;
        for row in table.rows() {
            if let Some(set) = src.prior_for(row.freq.lo_hz, row.freq.hi_hz) {
                any = true;
                assert!(set.validate().is_ok(), "{}: {set:?}", row.id);
            }
        }
        assert!(any, "expected at least one row to carry a mapped prior");
    }
}
