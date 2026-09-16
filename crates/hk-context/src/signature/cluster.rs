//! C18 clustering of unknown emissions (T-202, ADR-0016 §5): "the same thing I saw before".
//!
//! # What this does
//!
//! Emissions the catalogue cannot identify are grouped by **how they measure**, so an operator can
//! see that an unknown burst is the fourth sighting of something already catalogued as unknown
//! rather than a fresh mystery. Two pieces:
//!
//! - **Online leader assignment** ([`assign_emitter`]): each newly measured emitter joins the
//!   nearest cluster within the tolerance-normalised distance ε = 1, seeds one, or is left
//!   deliberately unassigned. O(clusters) per emitter, no batch pass needed to make progress.
//! - **Batch repair** ([`repair`]): a DBSCAN (ε = 1, minPts = 3) over current features re-derives
//!   the partition on demand (nightly), and the differences become append-only merge / split /
//!   reassign history. This is what fixes the fragmentation the online pass leaves behind.
//!
//! # Evidence, never identity
//!
//! A cluster sets nothing on an emitter: not identity, not family, not `known_status`, not
//! lifecycle. Membership means "this measures like those", never "this *is* X" — only a CRC-valid
//! decode confirms a signal. [`promote`] turns a cluster into a [`Signature`], and even that is a
//! ranked hypothesis the matcher scores like any other catalogue entry. The tests in
//! [`super::cluster_tests`] pin this.
//!
//! # The separation guard, and why it is unconditional
//!
//! Two genuinely distinct emitters sharing one cluster id is the worst outcome available here: it
//! invents a relationship, and it hides one emission behind another. Two ids for one emitter is
//! visible fragmentation the repair pass can fix. So [`compare`] refuses a join whenever
//!
//! 1. **any** comparable field actively disagrees (`z > 3`) — checked over every shared field,
//!    *before* and regardless of how many fields the two share, so one flat contradiction (a
//!    different modulation family, a different sync word, 2 levels against 4) separates the pair
//!    even when it is the only thing both have measured; then
//! 2. fewer than [`CLUSTER_MIN_SHARED_FIELDS`] fields are comparable at all — a nearly empty
//!    feature vector is close to *everything*, so it joins nothing and waits for more measurement;
//!    then
//! 3. the RMS distance over the shared fields exceeds ε.
//!
//! Rule 1 before rule 2 is the point: a conflict is a reason to separate, never something a thin
//! measurement can dodge.
//!
//! # How the T-201 uncertainty rule is respected
//!
//! Each side's `sigma` is `max(spread, sigma_meas)` and never shrinks as `1/√n` (see
//! [`hk_model::Feat`]). The distance widens the tolerance by **both** sides' sigmas in quadrature,
//! `eff = √(tol² + σa² + σb²)`, exactly as the matcher widens a catalogue tolerance by one
//! measurement's sigma. So a drifting oscillator or a deviation re-estimated from short bursts
//! stays *inside* its own cluster instead of drifting out of it, and a badly measured field can
//! neither manufacture a split nor — since it also cannot reach `z ≤ 1` on its own — force a
//! merge. Centroids fold members the same way ([`hk_model::ClusterCentroid`]), so the rule holds at
//! the type level too.

use std::collections::{BTreeMap, BTreeSet};

use hk_model::signature::Z_CONFLICT;
use hk_model::signature::cluster::{
    CLUSTER_MIN_APPEARANCES, CLUSTER_MIN_MEMBERS, ClusterCentroid, ClusterEvent, ClusterEventKind,
    ClusterState, EmitterClusterLink, SignatureCluster, is_cluster_field, new_cluster_id,
};
use hk_model::{
    EmissionFeatures, EmitterId, Feat, FeatValue, MatchOutcome, RepoError, Repository, Signature,
    Timestamp,
};

use super::matcher::{TEXT_MISMATCH_Z, bit_errors, default_tolerance};

/// Join radius in the tolerance-normalised metric: `z ≤ 1` is agreement whatever the units, so
/// ε = 1 is "within tolerance on the fields both measured" (ADR-0016 §5).
pub const CLUSTER_EPSILON: f64 = 1.0;

/// Fewest comparable fields a pair needs before their distance means anything. Below this the
/// clusterer abstains: a vector with one or two fields is near everything.
pub const CLUSTER_MIN_SHARED_FIELDS: usize = 3;

/// DBSCAN `minPts` for the repair pass, counting the point itself (ADR-0016 §5).
pub const REPAIR_MIN_POINTS: usize = 3;

/// Why two measurements may not share a cluster.
#[derive(Clone, Debug, PartialEq)]
pub enum Separated {
    /// One field actively disagrees (`z > 3`). Checked however few fields are shared.
    Conflict {
        /// The field that disagrees.
        field: String,
        /// Its normalised distance.
        z: f64,
    },
    /// Too little is comparable for the distance to mean anything.
    TooFewShared {
        /// Fields both sides measured.
        shared: usize,
    },
    /// Comparable, but further apart than ε.
    TooFar {
        /// RMS normalised distance.
        z_rms: f64,
        /// Fields both sides measured.
        shared: usize,
    },
}

impl Separated {
    /// Machine reason code, as stored on a membership row.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "conflict",
            Self::TooFewShared { .. } => "too_few_fields",
            Self::TooFar { .. } => "too_far",
        }
    }
}

/// How close two measurements are, when they may share a cluster at all.
#[derive(Clone, Debug, PartialEq)]
pub struct Closeness {
    /// RMS normalised distance over the shared fields (≤ [`CLUSTER_EPSILON`]).
    pub z_rms: f64,
    /// Fields both sides measured.
    pub shared: usize,
    /// The field that agrees least, and its `z`.
    pub worst: Option<(String, f64)>,
}

/// The tolerance-normalised distance of one field, or `None` when the two are not comparable
/// (a number against a label). Absence and incomparability are never disagreement.
fn field_z(name: &str, a: &Feat, b: &Feat) -> Option<f64> {
    let z = match (&a.value, &b.value) {
        (FeatValue::Num { value: x }, FeatValue::Num { value: y }) => {
            let scale = (x.abs() + y.abs()) / 2.0;
            let relative = default_tolerance(name);
            // A tolerance relative to zero is meaningless; fall back to an absolute one.
            let tolerance = if scale > 0.0 {
                relative * scale
            } else {
                relative
            };
            // Both measurements' uncertainties widen the tolerance, in quadrature. This is the
            // T-201 rule applied symmetrically: neither side is claimed to be better known than
            // it is, so a drifting emission does not split away from itself.
            let effective = (tolerance * tolerance + a.sigma * a.sigma + b.sigma * b.sigma)
                .sqrt()
                .max(f64::MIN_POSITIVE);
            (x - y).abs() / effective
        }
        (FeatValue::Bits { bits: x }, FeatValue::Bits { bits: y }) => {
            let (long, short) = if x.len() >= y.len() { (x, y) } else { (y, x) };
            let errors = bit_errors(long, short)?;
            // One bit error in eight is the budget: a sync word read with the odd slicer error is
            // the same sync word, a different one is not.
            let budget = (short.len() as f64 / 8.0).max(1.0);
            f64::from(errors) / budget
        }
        (FeatValue::Text { text: x }, FeatValue::Text { text: y }) => {
            if x.trim().eq_ignore_ascii_case(y.trim()) {
                0.0
            } else {
                TEXT_MISMATCH_Z
            }
        }
        _ => return None,
    };
    Some(if z.is_finite() { z } else { TEXT_MISMATCH_Z })
}

/// Compares two measurements over [`hk_model::CLUSTER_FIELDS`], with the guard described in the
/// module docs. `Ok` means they may share a cluster.
pub fn compare(
    a: &BTreeMap<String, Feat>,
    b: &BTreeMap<String, Feat>,
) -> Result<Closeness, Separated> {
    let mut zs: Vec<f64> = Vec::new();
    let mut worst: Option<(String, f64)> = None;
    for (name, fa) in a {
        if !is_cluster_field(name) {
            continue;
        }
        let Some(fb) = b.get(name) else { continue };
        let Some(z) = field_z(name, fa, fb) else {
            continue;
        };
        // Unconditional: one flat contradiction separates the pair whatever else is shared, and
        // whether or not enough fields are shared for a distance to be computed at all.
        if z > Z_CONFLICT {
            return Err(Separated::Conflict {
                field: name.clone(),
                z,
            });
        }
        if worst.as_ref().is_none_or(|(_, w)| z > *w) {
            worst = Some((name.clone(), z));
        }
        zs.push(z);
    }
    if zs.len() < CLUSTER_MIN_SHARED_FIELDS {
        return Err(Separated::TooFewShared { shared: zs.len() });
    }
    let z_rms = (zs.iter().map(|z| z * z).sum::<f64>() / zs.len() as f64).sqrt();
    if z_rms > CLUSTER_EPSILON {
        return Err(Separated::TooFar {
            z_rms,
            shared: zs.len(),
        });
    }
    Ok(Closeness {
        z_rms,
        shared: zs.len(),
        worst,
    })
}

/// The clustering fields of one features snapshot.
pub fn comparable(features: &EmissionFeatures) -> BTreeMap<String, Feat> {
    features
        .fields
        .iter()
        .filter(|(name, _)| is_cluster_field(name))
        .map(|(name, feat)| (name.clone(), feat.clone()))
        .collect()
}

/// What the online pass decided about one emitter.
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    /// The (live) emitter.
    pub emitter_id: EmitterId,
    /// The cluster it now belongs to, or `None` when the clusterer abstained.
    pub cluster_id: Option<String>,
    /// That cluster's state, when there is one.
    pub state: Option<ClusterState>,
    /// Machine reason code (`joined`, `seeded`, `reassigned`, `ambiguous`, `too_few_fields`,
    /// `conflict`, `too_far`, `identified`).
    pub reason: &'static str,
    /// The distance that decided it, when one was computed.
    pub distance: Option<f64>,
    /// Whether the membership changed (a row was appended).
    pub changed: bool,
}

/// Assigns one emitter to a cluster of unknowns, online.
///
/// Returns `None` when the emitter has no features snapshot: nothing measured is nothing to say,
/// which is not the same as "belongs nowhere" and is not recorded as one.
///
/// An emitter the catalogue matches `full` is left alone (`identified`): clustering exists for the
/// emissions nothing explains yet (ADR-0016 §5). A `partial` match is *not* an identification, so
/// those are clustered like anything else.
pub fn assign_emitter(
    repo: &mut Repository,
    emitter_id: EmitterId,
    t: Timestamp,
) -> Result<Option<Assignment>, RepoError> {
    let live = repo.live_emitter_id(emitter_id)?;
    let Some(features) = repo.emitter_features(live)? else {
        return Ok(None);
    };
    let current = repo.emitter_cluster_id(live)?;
    if repo
        .current_signature_match(live)?
        .is_some_and(|m| m.outcome == MatchOutcome::Full)
    {
        return Ok(Some(Assignment {
            emitter_id: live,
            cluster_id: current,
            state: None,
            reason: "identified",
            distance: None,
            changed: false,
        }));
    }

    let mine = comparable(&features);
    let open = repo.open_clusters()?;
    let mut accepted: Vec<(SignatureCluster, f64)> = Vec::new();
    let mut nearest_miss: Option<Separated> = None;
    for c in open {
        match compare(c.centroid.comparable(), &mine) {
            Ok(close) => accepted.push((c, close.z_rms)),
            Err(sep) => {
                if nearest_miss.is_none() {
                    nearest_miss = Some(sep);
                }
            }
        }
    }
    accepted.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.id.cmp(&b.0.id)));

    // More than one cluster accepts. That is only safe when those clusters are themselves
    // compatible — near-duplicates of one type, which the repair pass will fold together. If they
    // are *not* mutually compatible, this emitter sits between two distinct types and joining
    // either would invent a relationship: abstain and let more measurement, or the repair pass,
    // decide.
    if accepted.len() > 1 && !mutually_compatible(&accepted) {
        return Ok(Some(abstain(
            repo,
            live,
            t,
            "ambiguous",
            Some(accepted[0].1),
            current,
        )?));
    }

    let (chosen, distance, reason) = match accepted.first() {
        Some((c, d)) => (c.id.clone(), Some(*d), "joined"),
        None => {
            if mine.len() < CLUSTER_MIN_SHARED_FIELDS {
                let reason = nearest_miss
                    .as_ref()
                    .map_or("too_few_fields", |s| s.reason());
                let reason = if reason == "conflict" {
                    "too_few_fields"
                } else {
                    reason
                };
                return Ok(Some(abstain(repo, live, t, reason, None, current)?));
            }
            let id = new_cluster_id();
            let c = SignatureCluster::new(&id, t);
            repo.put_cluster(&c)?;
            repo.append_cluster_event(&ClusterEvent::new(&id, ClusterEventKind::Created, t))?;
            (id, None, "seeded")
        }
    };

    let changed = current.as_deref() != Some(chosen.as_str());
    let reason = if changed && current.is_some() {
        "reassigned"
    } else {
        reason
    };
    if changed {
        repo.link_emitter_cluster(&EmitterClusterLink {
            emitter_id: live,
            cluster_id: Some(chosen.clone()),
            t,
            reason: reason.to_owned(),
            distance,
        })?;
        if let Some(from) = &current {
            repo.append_cluster_event(&ClusterEvent {
                cluster_id: chosen.clone(),
                kind: ClusterEventKind::Reassign,
                other_cluster_id: Some(from.clone()),
                emitter_id: Some(live),
                t,
                detail: serde_json::json!({ "distance": distance }),
            })?;
        }
    }

    // Fold what this emitter measures like into the cluster's centroid, carrying its uncertainty.
    let mut cluster = repo.cluster(&chosen)?;
    cluster
        .centroid
        .fold_member(&mine, features.suspect_fraction);
    cluster.touch(t);
    repo.put_cluster(&cluster)?;
    let state = refresh_state(repo, &chosen, t)?;

    Ok(Some(Assignment {
        emitter_id: live,
        cluster_id: Some(chosen),
        state: Some(state),
        reason,
        distance,
        changed,
    }))
}

/// Whether every accepting cluster would also accept every other: near-duplicates of one type.
fn mutually_compatible(accepted: &[(SignatureCluster, f64)]) -> bool {
    for (i, (a, _)) in accepted.iter().enumerate() {
        for (b, _) in &accepted[i + 1..] {
            if compare(a.centroid.comparable(), b.centroid.comparable()).is_err() {
                return false;
            }
        }
    }
    true
}

/// Records that the clusterer looked and declined to place this emitter.
fn abstain(
    repo: &mut Repository,
    emitter_id: EmitterId,
    t: Timestamp,
    reason: &'static str,
    distance: Option<f64>,
    current: Option<String>,
) -> Result<Assignment, RepoError> {
    let changed = current.is_some();
    if changed || repo.emitter_cluster(emitter_id)?.is_none() {
        repo.link_emitter_cluster(&EmitterClusterLink {
            emitter_id,
            cluster_id: None,
            t,
            reason: reason.to_owned(),
            distance,
        })?;
    }
    if let Some(from) = current {
        repo.append_cluster_event(&ClusterEvent {
            cluster_id: from,
            kind: ClusterEventKind::Reassign,
            other_cluster_id: None,
            emitter_id: Some(emitter_id),
            t,
            detail: serde_json::json!({ "reason": reason }),
        })?;
    }
    Ok(Assignment {
        emitter_id,
        cluster_id: None,
        state: None,
        reason,
        distance,
        changed,
    })
}

/// Recomputes a cluster's visibility: at least [`CLUSTER_MIN_MEMBERS`] members, or one member seen
/// in at least [`CLUSTER_MIN_APPEARANCES`] separated appearances ("the same thing I saw before"
/// holds for one emitter seen again and again, ADR-0016 §5).
fn refresh_state(
    repo: &mut Repository,
    cluster_id: &str,
    t: Timestamp,
) -> Result<ClusterState, RepoError> {
    let mut cluster = repo.cluster(cluster_id)?;
    if matches!(cluster.state, ClusterState::Merged | ClusterState::Promoted) {
        return Ok(cluster.state);
    }
    let members = repo.cluster_members(cluster_id)?;
    let mut visible = members.len() >= CLUSTER_MIN_MEMBERS;
    if !visible && members.len() == 1 {
        let recurrence = repo.emitter_recurrence(members[0], 0)?;
        visible = recurrence.appearances >= u64::from(CLUSTER_MIN_APPEARANCES);
    }
    let next = if visible {
        ClusterState::Active
    } else {
        ClusterState::Pending
    };
    if next != cluster.state {
        cluster.state = next;
        cluster.touch(t);
        repo.put_cluster(&cluster)?;
        if next == ClusterState::Active {
            repo.append_cluster_event(&ClusterEvent {
                cluster_id: cluster_id.to_owned(),
                kind: ClusterEventKind::Activated,
                other_cluster_id: None,
                emitter_id: None,
                t,
                detail: serde_json::json!({ "members": members.len() }),
            })?;
        }
    }
    Ok(next)
}

/// What a repair pass changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    /// Emitters with enough measurement to take part.
    pub points: usize,
    /// Groups the batch pass found.
    pub groups: usize,
    /// Clusters folded into another.
    pub merges: usize,
    /// Clusters whose members ended up in more than one group.
    pub splits: usize,
    /// Memberships that changed.
    pub reassignments: usize,
    /// Points left in no cluster.
    pub unassigned: usize,
}

/// Re-derives the partition with DBSCAN (ε = 1, `minPts` = 3) over current features and records
/// the differences as append-only history (ADR-0016 §5: the nightly repair).
///
/// The neighbourhood test is the same guarded [`compare`] the online pass uses, so a pair that may
/// not share a cluster is not a neighbour — the separation guard holds in both passes. Ids of the
/// larger side survive, so a cluster id an operator has been watching keeps its meaning.
pub fn repair(repo: &mut Repository, t: Timestamp) -> Result<RepairReport, RepoError> {
    let mut report = RepairReport::default();

    // The points: every emitter the clusterer has decided about, with enough measured to compare.
    let mut ids: Vec<EmitterId> = Vec::new();
    let mut feats: Vec<BTreeMap<String, Feat>> = Vec::new();
    let mut suspect: Vec<f64> = Vec::new();
    let mut before: BTreeMap<EmitterId, Option<String>> = BTreeMap::new();
    let mut stragglers: Vec<EmitterId> = Vec::new();
    for link in repo.clustered_emitters()? {
        let live = repo.live_emitter_id(link.emitter_id)?;
        let current = match link.cluster_id.as_deref() {
            Some(c) => Some(repo.live_cluster_id(c)?),
            None => None,
        };
        before.insert(live, current);
        let Some(f) = repo.emitter_features(live)? else {
            stragglers.push(live);
            continue;
        };
        let fields = comparable(&f);
        if fields.len() < CLUSTER_MIN_SHARED_FIELDS {
            stragglers.push(live);
            continue;
        }
        ids.push(live);
        feats.push(fields);
        suspect.push(f.suspect_fraction);
    }
    report.points = ids.len();

    // Neighbourhoods under the guarded distance.
    let n = ids.len();
    let mut neighbours: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        for j in (i + 1)..n {
            if compare(&feats[i], &feats[j]).is_ok() {
                neighbours[i].push(j);
                neighbours[j].push(i);
            }
        }
    }

    // DBSCAN: core points (counting themselves) expand; border points join without expanding.
    let core = |i: usize| neighbours[i].len() + 1 >= REPAIR_MIN_POINTS;
    let mut group_of: Vec<Option<usize>> = vec![None; n];
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for seed in 0..n {
        if group_of[seed].is_some() || !core(seed) {
            continue;
        }
        let g = groups.len();
        let mut members = vec![seed];
        group_of[seed] = Some(g);
        let mut queue = vec![seed];
        while let Some(i) = queue.pop() {
            for &j in &neighbours[i] {
                if group_of[j].is_some() {
                    continue;
                }
                group_of[j] = Some(g);
                members.push(j);
                if core(j) {
                    queue.push(j);
                }
            }
        }
        members.sort_unstable();
        groups.push(members);
    }
    report.groups = groups.len();

    // Each group keeps the id most of its members already carry (ties: the oldest id, since ids
    // are time-ordered). Groups are served strongest claim first and **an id can be claimed only
    // once**: when one cluster turns out to hold two different things, the larger side keeps the
    // id and the other becomes a new cluster, rather than both answering to the same name.
    let mut claims: Vec<(usize, Option<(String, usize)>)> = Vec::new();
    for (g, members) in groups.iter().enumerate() {
        let mut votes: BTreeMap<String, usize> = BTreeMap::new();
        for &i in members {
            if let Some(Some(c)) = before.get(&ids[i]) {
                *votes.entry(c.clone()).or_default() += 1;
            }
        }
        let best = votes
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(id, n)| (id.clone(), *n));
        claims.push((g, best));
    }
    claims.sort_by(|a, b| {
        let (na, nb) = (
            a.1.as_ref().map_or(0, |(_, n)| *n),
            b.1.as_ref().map_or(0, |(_, n)| *n),
        );
        nb.cmp(&na).then_with(|| a.0.cmp(&b.0))
    });
    let mut survivors: Vec<String> = vec![String::new(); groups.len()];
    let mut taken: BTreeSet<String> = BTreeSet::new();
    for (g, best) in claims {
        let id = match best {
            Some((id, _)) if !taken.contains(&id) => id,
            _ => {
                let id = new_cluster_id();
                repo.put_cluster(&SignatureCluster::new(&id, t))?;
                repo.append_cluster_event(&ClusterEvent::new(&id, ClusterEventKind::Created, t))?;
                id
            }
        };
        taken.insert(id.clone());
        survivors[g] = id;
    }

    // Where each old cluster's members went, for the merge/split history.
    let mut went: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (g, members) in groups.iter().enumerate() {
        for &i in members {
            if let Some(Some(from)) = before.get(&ids[i]) {
                went.entry(from.clone())
                    .or_default()
                    .insert(survivors[g].clone());
            }
        }
    }

    // Rewrite the memberships that changed.
    for (g, members) in groups.iter().enumerate() {
        let to = &survivors[g];
        for &i in members {
            let id = ids[i];
            let from = before.get(&id).cloned().flatten();
            if from.as_deref() == Some(to.as_str()) {
                continue;
            }
            repo.link_emitter_cluster(&EmitterClusterLink {
                emitter_id: id,
                cluster_id: Some(to.clone()),
                t,
                reason: "repair".into(),
                distance: None,
            })?;
            repo.append_cluster_event(&ClusterEvent {
                cluster_id: to.clone(),
                kind: ClusterEventKind::Reassign,
                other_cluster_id: from,
                emitter_id: Some(id),
                t,
                detail: serde_json::json!({ "pass": "repair" }),
            })?;
            report.reassignments += 1;
        }
    }

    // Noise: a point in no group keeps nothing. Abstaining is the honest answer — DBSCAN found
    // too few neighbours to call it the same thing as anything else.
    for i in 0..n {
        if group_of[i].is_some() {
            continue;
        }
        let id = ids[i];
        if before.get(&id).cloned().flatten().is_some() {
            repo.link_emitter_cluster(&EmitterClusterLink {
                emitter_id: id,
                cluster_id: None,
                t,
                reason: "repair".into(),
                distance: None,
            })?;
            report.reassignments += 1;
        }
        report.unassigned += 1;
    }
    for id in stragglers {
        if before.get(&id).cloned().flatten().is_some() {
            repo.link_emitter_cluster(&EmitterClusterLink {
                emitter_id: id,
                cluster_id: None,
                t,
                reason: "repair".into(),
                distance: None,
            })?;
            report.reassignments += 1;
            report.unassigned += 1;
        }
    }

    // Merge and split history for the clusters that lost members.
    for (from, to) in &went {
        if to.len() > 1 {
            for dest in to {
                if dest != from {
                    repo.append_cluster_event(&ClusterEvent {
                        cluster_id: from.clone(),
                        kind: ClusterEventKind::Split,
                        other_cluster_id: Some(dest.clone()),
                        emitter_id: None,
                        t,
                        detail: serde_json::json!({ "pass": "repair" }),
                    })?;
                }
            }
            report.splits += 1;
        }
        if repo.cluster_member_count(from)? == 0
            && let Some(dest) = to.iter().find(|d| *d != from)
            && to.len() == 1
        {
            let mut loser = repo.cluster(from)?;
            if loser.state != ClusterState::Promoted {
                loser.state = ClusterState::Merged;
                loser.merged_into = Some(dest.clone());
                loser.touch(t);
                repo.put_cluster(&loser)?;
                repo.append_cluster_event(&ClusterEvent {
                    cluster_id: dest.clone(),
                    kind: ClusterEventKind::Merge,
                    other_cluster_id: Some(from.clone()),
                    emitter_id: None,
                    t,
                    detail: serde_json::json!({ "pass": "repair" }),
                })?;
                report.merges += 1;
            }
        }
    }

    // Recompute every surviving centroid from its members, in id order, so the result of a repair
    // depends only on what is stored — not on the order sightings happened to arrive.
    let mut touched: BTreeSet<String> = survivors.iter().cloned().collect();
    touched.extend(went.keys().cloned());
    for id in touched {
        let Some(mut cluster) = repo.cluster_opt(&id)? else {
            continue;
        };
        if cluster.state == ClusterState::Merged {
            continue;
        }
        let members = repo.cluster_members(&id)?;
        let mut centroid = ClusterCentroid::new();
        for m in &members {
            if let Some(f) = repo.emitter_features(*m)? {
                centroid.fold_member(&comparable(&f), f.suspect_fraction);
            }
        }
        cluster.centroid = centroid;
        cluster.touch(t);
        repo.put_cluster(&cluster)?;
        refresh_state(repo, &id, t)?;
    }

    Ok(report)
}

/// Turns a cluster into a [`Signature`] with provenance `cluster-promoted` (ADR-0016 §5).
///
/// This is still **evidence**: the minted entry is a hypothesis the matcher scores like any other
/// catalogue row, and a match against it sets no identity. Refused when the cluster is not visible
/// yet, when every observation behind it was suspect (the front end makes ghosts with real-looking
/// parameters), or when fewer than three discriminating fields were measured — the same floor the
/// matcher enforces before it will call anything a `full` match.
pub fn promote(
    repo: &mut Repository,
    cluster_id: &str,
    author: &str,
    t: Timestamp,
) -> Result<Signature, RepoError> {
    repo.promote_cluster(cluster_id, author, t)
}
