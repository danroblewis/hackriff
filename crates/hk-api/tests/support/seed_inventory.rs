//! Test-only inventory seed (T-022 fixtures for the hk-api inventory tests). Not a demo and not
//! reachable from any binary: `hk serve` and `hackriffd` show only what their running pipeline
//! detected (T-042).
//!
//! Every row goes through `Repository::record_sighting` (entity resolution + known-status
//! priors), like the hk-detect track adapter, except one legacy unclassified identity. `T0` defaults
//! to 2026-09-13T11:10:00Z, the FM fixture's capture time.

use hk_model::{
    Classification, ContentClass, DecodeId, DecodedIdentity, EmitterId, EmitterObservation,
    Fingerprint, IdentityClaim, IdentityScheme, KnownStatus, LinkTarget, PriorVerdict, RepoError,
    Repository, Sighting, TimeRange, Timestamp, TrackId,
};

/// 2026-09-13T11:10:00Z.
pub const DEFAULT_T0_S: i64 = 1_789_297_800;
/// RDS PI of the unrestricted broadcast emitter (shown in clear).
pub const RDS_PI: &str = "C0DE";
/// Pager capcode (restricted-paging class): must never appear in API output.
pub const PAGER_CAPCODE: &str = "7654321";
/// Own-key-decrypted sensor id: withheld over HTTP (no own-traffic authorisation in M0).
pub const OWN_SENSOR_ID: &str = "own-7f3a91";
/// Talkgroup written by a legacy writer with no class: withheld (fail closed).
pub const LEGACY_TALKGROUP: &str = "sys9:4242";

/// Ids of the seeded emitters.
#[derive(Clone, Copy, Debug)]
pub struct Seeded {
    /// 100.8 MHz broadcast FM, RDS PI in clear, known (band plan).
    pub rds: EmitterId,
    /// 931.9375 MHz pager, capcode withheld.
    pub pager: EmitterId,
    /// 868.3 MHz own sensor, withheld.
    pub own: EmitterId,
    /// 433.92 MHz ISM weather sensor, id in clear, known.
    pub sensor: EmitterId,
    /// 446.1 MHz anonymous FSK, unexpected here, seen an hour before `T0`.
    pub fsk: EmitterId,
    /// 145.8 MHz anonymous carrier, unknown.
    pub carrier: EmitterId,
    /// 851 MHz legacy talkgroup, unclassified, withheld.
    pub legacy: EmitterId,
}

impl Seeded {
    /// All ids.
    pub fn all(&self) -> [EmitterId; 7] {
        [
            self.rds,
            self.pager,
            self.own,
            self.sensor,
            self.fsk,
            self.carrier,
            self.legacy,
        ]
    }
}

/// A band-plan stand-in (hk-context's `match_known_status` fits the same trait).
fn prior(family: &str, f: f64, _bw: f64) -> PriorVerdict {
    let verdict = |status, prior_ref: Option<&str>, reason: &str| PriorVerdict {
        status,
        prior_ref: prior_ref.map(str::to_owned),
        reason: reason.to_owned(),
    };
    match family {
        "wfm" if (87.5e6..=108e6).contains(&f) => verdict(
            KnownStatus::Known,
            Some("bandplan:demo#87.5-108MHz"),
            "broadcast FM allocation",
        ),
        "fsk2" if (433.05e6..=434.79e6).contains(&f) => verdict(
            KnownStatus::Known,
            Some("bandplan:demo#433.05-434.79MHz"),
            "ISM 433 MHz short-range devices",
        ),
        "fsk2" if (446.0e6..=446.2e6).contains(&f) => verdict(
            KnownStatus::UnexpectedHere,
            Some("bandplan:demo#446.0-446.2MHz"),
            "data burst in the PMR446 analogue voice allocation",
        ),
        _ => verdict(KnownStatus::Unknown, None, "no band-plan match"),
    }
}

fn ts(s: i64) -> Timestamp {
    Timestamp::from_unix_nanos(s * 1_000_000_000)
}

struct Row<'a> {
    f: f64,
    bw: f64,
    family: Option<&'a str>,
    identity: Option<(IdentityScheme, &'a str, ContentClass)>,
    seen: (i64, i64),
    count: u64,
    tags: &'a [&'a str],
}

fn record(repo: &mut Repository, r: Row<'_>) -> Result<EmitterId, RepoError> {
    let mut fp = Fingerprint::new(r.f, r.bw);
    fp.family = r.family.map(str::to_owned);
    let sighting = Sighting {
        source: if r.identity.is_some() {
            LinkTarget::Decode(DecodeId::new())
        } else {
            LinkTarget::Track(TrackId::new())
        },
        seen: TimeRange::new(ts(r.seen.0), ts(r.seen.1)),
        count: r.count,
        f_center_hz: r.f,
        bandwidth_hz: r.bw,
        fingerprint: Some(fp),
        identity: r.identity.map(|(scheme, value, class)| IdentityClaim {
            identity: DecodedIdentity {
                scheme,
                value: value.to_owned(),
            },
            content_class: class,
        }),
        context: None,
        classification: r.family.map(|family| Classification {
            t: ts(r.seen.1),
            family: family.to_owned(),
            confidence: 0.9,
            open_set_score: 0.1,
            model_version: "demo-seed@0".into(),
        }),
        tags: r.tags.iter().map(|t| (*t).to_owned()).collect(),
    };
    Ok(repo.record_sighting(&sighting, Some(&prior))?.emitter_id)
}

/// Writes the demo emitters around `t0_s` (Unix seconds).
pub fn seed(repo: &mut Repository, t0_s: i64) -> Result<Seeded, RepoError> {
    let t = t0_s;
    let rds = record(
        repo,
        Row {
            f: 100.8e6,
            bw: 180e3,
            family: Some("wfm"),
            identity: Some((IdentityScheme::RdsPi, RDS_PI, ContentClass::Unrestricted)),
            seen: (t, t + 5),
            count: 5,
            tags: &["broadcast"],
        },
    )?;
    let pager = record(
        repo,
        Row {
            f: 931.9375e6,
            bw: 25e3,
            family: Some("fsk2"),
            identity: Some((
                IdentityScheme::Other("pocsag-capcode".into()),
                PAGER_CAPCODE,
                ContentClass::RestrictedPaging,
            )),
            seen: (t - 600, t - 590),
            count: 3,
            tags: &["pager"],
        },
    )?;
    let own = record(
        repo,
        Row {
            f: 868.3e6,
            bw: 50e3,
            family: None,
            identity: Some((
                IdentityScheme::Other("own-sensor".into()),
                OWN_SENSOR_ID,
                ContentClass::OwnKeyDecrypted,
            )),
            seen: (t - 300, t - 120),
            count: 4,
            tags: &["mine"],
        },
    )?;
    let sensor = record(
        repo,
        Row {
            f: 433.92e6,
            bw: 30e3,
            family: Some("fsk2"),
            identity: Some((
                IdentityScheme::SensorId,
                "acurite-tower:77",
                ContentClass::Unrestricted,
            )),
            seen: (t - 900, t + 60),
            count: 12,
            tags: &["ism", "weather"],
        },
    )?;
    let fsk = record(
        repo,
        Row {
            f: 446.1e6,
            bw: 12.5e3,
            family: Some("fsk2"),
            identity: None,
            seen: (t - 3600, t - 3540),
            count: 9,
            tags: &["review"],
        },
    )?;
    let carrier = record(
        repo,
        Row {
            f: 145.8e6,
            bw: 3e3,
            family: None,
            identity: None,
            seen: (t - 60, t + 30),
            count: 2,
            tags: &[],
        },
    )?;
    // A legacy writer: an identity with no class and no linked decode (withheld, fail closed).
    let legacy = repo
        .upsert_emitter_observation(&EmitterObservation {
            emitter_id: EmitterId::new(),
            seen: TimeRange::new(ts(t - 1200), ts(t - 1100)),
            count: 6,
            f_center_hz: 851e6,
            bandwidth_hz: 12.5e3,
            identity: Some(DecodedIdentity {
                scheme: IdentityScheme::Talkgroup,
                value: LEGACY_TALKGROUP.into(),
            }),
        })?
        .emitter_id;
    Ok(Seeded {
        rds,
        pager,
        own,
        sensor,
        fsk,
        carrier,
        legacy,
    })
}
