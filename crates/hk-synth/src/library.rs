//! The template library (ADR-0015 §4.1, §15.6 level 1): loader, consistency validation,
//! built-ins, and seeding (§4.2).
//!
//! **Templates order the search; they never rank or confirm** (§1.3, §4.1). Everything in
//! [`TemplateLibrary::seed`] produces `prior_bits` (an ordering key) and nothing else: a template
//! that does not fit the measurement is *demoted*, never dropped, and `bands_hz` only raises rank
//! for an emitter already detected there (`centre_hz` is the detection's, never a place to tune).
//!
//! Validation is quality control on the library, not a bit source (§15.6). A skeleton naming a
//! block the catalogue lacks makes its template **inert**: it loads, validates and is listed, but
//! seeding never offers it, and [`TemplateLibrary::inert`] reports the missing blocks so the
//! trace can say `missing_block` with `suspected_by: template` (§15.6).
//!
//! Not checked here, and why: the CRC `check`-value round trip and field-map width sums need
//! recipe/CRC parameters a skeleton does not carry, and "every `free` path names a
//! `protocol`-class param" waits on ADR-0011's `ParamSchema.class` (not landed). Both are listed
//! as follow-ups rather than half-done.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use hk_model::classify::{BudgetHint, Hypothesis, SeedBoost};
use hk_recipe::{Catalogue, Recipe};
use serde::{Deserialize, Serialize};

use crate::candidate::{Domain, SeedSource};
use crate::evidence::prior_bits;
use crate::skeleton::Skeleton;
use crate::stage::Stage;
use crate::template::{
    AuthorKind, TEMPLATE_SCHEMA, TEMPLATE_SCHEMA_VERSION, Template, TemplateStructure,
};

/// Built-in template documents, shipped in `templates/` and embedded so they are always present.
const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    (
        "pocsag",
        include_str!("../../../templates/pocsag.template.json"),
    ),
    (
        "acars",
        include_str!("../../../templates/acars.template.json"),
    ),
    (
        "adsb",
        include_str!("../../../templates/adsb.template.json"),
    ),
    ("rds", include_str!("../../../templates/rds.template.json")),
    (
        "generic-fsk-framed",
        include_str!("../../../templates/generic-fsk-framed.template.json"),
    ),
    (
        "generic-ook-pwm",
        include_str!("../../../templates/generic-ook-pwm.template.json"),
    ),
    (
        "generic-ook-manchester",
        include_str!("../../../templates/generic-ook-manchester.template.json"),
    ),
    (
        "generic-msk",
        include_str!("../../../templates/generic-msk.template.json"),
    ),
    (
        "generic-ppm",
        include_str!("../../../templates/generic-ppm.template.json"),
    ),
    (
        "generic-psk-framed",
        include_str!("../../../templates/generic-psk-framed.template.json"),
    ),
];

/// The shipped recipes the recipe-backed built-ins point at.
const BUILTIN_RECIPES: &[&str] = &[
    include_str!("../../../recipes/pocsag.recipe.json"),
    include_str!("../../../recipes/acars.recipe.json"),
    include_str!("../../../recipes/adsb.recipe.json"),
    include_str!("../../../recipes/rds.recipe.json"),
];

/// Machine-readable reason a template failed to load or validate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemplateErrorCode {
    /// Not JSON, or not this schema's shape (an unknown key is refused, never ignored).
    Parse,
    /// Wrong `schema` or `schema_version`.
    Schema,
    /// Not exactly one of `recipe` / `skeleton`.
    Structure,
    /// The referenced recipe id/version is not known.
    RecipeMissing,
    /// A `free` path is malformed or names a node the structure does not have.
    FreePath,
    /// A range, domain or weight list is incoherent.
    Range,
    /// `evidence_targets.S4.sync_bits` disagrees with the recipe's `sync_search`.
    SyncBits,
    /// A builtin fact-bearing field has no source (§15.2).
    FactUnsourced,
    /// The same `id@version` appeared twice (versions are immutable).
    Duplicate,
    /// A skeleton is empty or repeats an alternative id.
    Skeleton,
}

/// One load or validation failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateError {
    /// Where it came from: a file path or a builtin name.
    pub origin: String,
    /// What kind of failure.
    pub code: TemplateErrorCode,
    /// Human detail.
    pub detail: String,
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {:?}: {}", self.origin, self.code, self.detail)
    }
}

impl std::error::Error for TemplateError {}

fn err(origin: &str, code: TemplateErrorCode, detail: impl Into<String>) -> TemplateError {
    TemplateError {
        origin: origin.to_string(),
        code,
        detail: detail.into(),
    }
}

/// A loaded, validated template.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedTemplate {
    /// The document.
    pub template: Template,
    /// Blocks its skeleton names that the catalogue lacks. Non-empty ⇒ **inert**: never seeded.
    pub missing_blocks: Vec<String>,
}

impl LoadedTemplate {
    /// Never seeded: the catalogue cannot express it.
    pub fn is_inert(&self) -> bool {
        !self.missing_blocks.is_empty()
    }
}

/// The measured parameters a template's priors are read against (§4.1: measured, never assumed).
/// `None` = not measured, which is neutral rather than a mismatch.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeasuredParams {
    /// Symbol rate, Bd.
    pub symbol_rate_bd: Option<f64>,
    /// Occupied bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
    /// Bursty emission.
    pub bursty: Option<bool>,
    /// Centre of an emitter **already detected** — used only for `band_factor`, never to tune.
    pub centre_hz: Option<f64>,
}

/// A prior a measurement contradicts is multiplied by this: a demotion, never a deletion.
pub const MISMATCH_FACTOR: f64 = 0.25;

/// `band_factor` for an emitter detected inside a template's `bands_hz` (§4.2: ∈ [1, 1.5]).
pub const BAND_FACTOR_MAX: f64 = 1.5;

/// One template offered to the search, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateSeed {
    /// `id@version`.
    pub template: String,
    /// The `hk-mod@1` family this hypothesis is for.
    pub family: String,
    /// `signature` when a signature or cluster raised the family, else `classification`.
    pub seed_source: SeedSource,
    /// `log₂ π(h)`, π = P_class × match × band_factor. Orders; never ranks.
    pub prior_bits: f32,
    /// The family's ADR-0016 deferral (likelihood only). Deferred ≠ dropped.
    pub deferred: bool,
    /// A `full` signature match named this template's recipe: try first, at the signature's values.
    pub fast_path: bool,
}

/// What seeding produced.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TemplateSeeding {
    /// Templates in search order: fast path, then active by prior, then deferred by prior.
    pub ordered: Vec<TemplateSeed>,
    /// Generic skeletons the open search always gets, whatever the classification said (§4.2's
    /// open-search floor: priors never starve unknowns).
    pub open_skeletons: Vec<String>,
    /// Share of the evaluation budget the open search keeps: ≥ max(P(unknown), 0.2).
    pub open_search_min_share: f64,
}

/// The library.
#[derive(Clone, Debug, Default)]
pub struct TemplateLibrary {
    entries: BTreeMap<String, LoadedTemplate>,
}

fn node_ids(t: &Template, recipes: &[Recipe]) -> Result<BTreeSet<String>, TemplateError> {
    let o = t.key();
    match t
        .structure()
        .map_err(|e| err(&o, TemplateErrorCode::Structure, e.to_string()))?
    {
        TemplateStructure::Recipe(r) => recipes
            .iter()
            .find(|x| x.id == r.id && x.version == r.version)
            .map(|x| x.nodes.iter().map(|n| n.id.clone()).collect())
            .ok_or_else(|| {
                err(
                    &o,
                    TemplateErrorCode::RecipeMissing,
                    format!("{}@{}", r.id, r.version),
                )
            }),
        TemplateStructure::Skeleton(s) => Ok(s
            .slots
            .values()
            .flatten()
            .flat_map(|a| a.nodes.iter().map(|n| n.id.clone()))
            .collect()),
    }
}

/// `nodes[ID].params.NAME` → `ID`.
fn free_node(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("nodes[")?;
    let (id, tail) = rest.split_once("].params.")?;
    (!id.is_empty() && !tail.is_empty()).then_some(id)
}

fn range_ok(r: [f64; 2]) -> bool {
    r[0].is_finite() && r[1].is_finite() && r[0] <= r[1]
}

/// Fact-bearing fields present in `t` (§15.2), as the paths a [`crate::template::Fact`] names.
fn fact_fields(t: &Template) -> Vec<String> {
    let p = &t.priors;
    let mut v = Vec::new();
    if !p.families.is_empty() {
        v.push("priors.families".to_string());
    }
    if !p.symbol_rate_bd.is_empty() {
        v.push("priors.symbol_rate_bd".into());
    }
    if p.bandwidth_hz.is_some() {
        v.push("priors.bandwidth_hz".into());
    }
    if p.bursty.is_some() {
        v.push("priors.bursty".into());
    }
    if !p.bands_hz.is_empty() {
        v.push("priors.bands_hz".into());
    }
    for s in t.evidence_targets.keys() {
        if let Ok(serde_json::Value::String(s)) = serde_json::to_value(s) {
            v.push(format!("evidence_targets.{s}"));
        }
    }
    if !t.plausibility.is_empty() {
        v.push("plausibility".into());
    }
    for f in &t.free {
        if let Some(n) = free_node(&f.path) {
            v.push(format!("free[{n}].domain"));
        }
    }
    v
}

/// Consistency validation (§15.6 level 1). `recipes` resolves a recipe-backed template.
pub fn validate(
    origin: &str,
    t: &Template,
    recipes: &[Recipe],
    catalogue: &dyn Catalogue,
) -> Result<Vec<String>, TemplateError> {
    if t.schema != TEMPLATE_SCHEMA || t.schema_version != TEMPLATE_SCHEMA_VERSION {
        return Err(err(
            origin,
            TemplateErrorCode::Schema,
            format!("{}/{}", t.schema, t.schema_version),
        ));
    }
    let ids = node_ids(t, recipes).map_err(|mut e| {
        e.origin = origin.to_string();
        e
    })?;
    for f in &t.free {
        match free_node(&f.path) {
            Some(n) if ids.contains(n) => {}
            _ => return Err(err(origin, TemplateErrorCode::FreePath, f.path.clone())),
        }
        let ok = match &f.domain {
            Domain::Float(d) => range_ok([d.lo, d.hi]),
            Domain::Int(d) => d.lo <= d.hi,
            Domain::Enum(d) => {
                !d.values.is_empty() && d.weights.as_ref().is_none_or(|w| w.len() == d.values.len())
            }
            Domain::Hex(d) => !d.candidates.is_empty(),
            Domain::Proposal(_) => true,
        };
        if !ok {
            return Err(err(
                origin,
                TemplateErrorCode::Range,
                format!("free {}", f.path),
            ));
        }
    }
    let p = &t.priors;
    let ranges = p
        .symbol_rate_bd
        .iter()
        .chain(&p.bands_hz)
        .copied()
        .chain(p.bandwidth_hz)
        .chain(t.plausibility.iter().map(|x| x.range));
    if ranges.into_iter().any(|r| !range_ok(r))
        || p.families.values().any(|w| !w.is_finite() || *w < 0.0)
    {
        return Err(err(
            origin,
            TemplateErrorCode::Range,
            "prior or plausibility range",
        ));
    }
    if let (Some(rr), Some(s4)) = (&t.recipe, t.evidence_targets.get(&Stage::S4))
        && let (Some(want), Some(rec)) = (
            s4.sync_bits,
            recipes
                .iter()
                .find(|x| x.id == rr.id && x.version == rr.version),
        )
    {
        for n in rec.nodes.iter().filter(|n| n.block == "sync_search") {
            if let Some(got) = n.params.get("sync_bits").and_then(|v| v.as_u64())
                && got != u64::from(want)
            {
                return Err(err(
                    origin,
                    TemplateErrorCode::SyncBits,
                    format!("template {want}, recipe node {} has {got}", n.id),
                ));
            }
        }
    }
    if let Some(s) = &t.skeleton {
        if s.slots.is_empty() {
            return Err(err(origin, TemplateErrorCode::Skeleton, "no slots"));
        }
        for (stage, alts) in &s.slots {
            let mut seen = BTreeSet::new();
            if alts.is_empty() || !alts.iter().all(|a| seen.insert(&a.id)) {
                return Err(err(
                    origin,
                    TemplateErrorCode::Skeleton,
                    format!("{stage:?}: empty or repeated alternative"),
                ));
            }
        }
    }
    if t.provenance.kind == AuthorKind::Builtin {
        let covered: BTreeSet<&str> = t
            .provenance
            .facts
            .iter()
            .flat_map(|f| f.fields.iter().map(String::as_str))
            .collect();
        if let Some(missing) = fact_fields(t)
            .into_iter()
            .find(|f| !covered.contains(f.as_str()))
        {
            return Err(err(origin, TemplateErrorCode::FactUnsourced, missing));
        }
    }
    let mut missing: Vec<String> = t
        .skeleton
        .iter()
        .flat_map(|s| s.slots.values().flatten())
        .flat_map(|a| a.nodes.iter())
        .filter(|n| catalogue.descriptor(&n.block).is_none())
        .map(|n| n.block.clone())
        .collect();
    missing.sort();
    missing.dedup();
    Ok(missing)
}

impl TemplateLibrary {
    /// The built-ins: one per shipped recipe plus the generic skeletons (`generic-psk-framed`
    /// included, so PSK can reach bits inside a search — T-856 note). Refuses to construct if
    /// any shipped document fails its own validation.
    pub fn builtin() -> Result<Self, TemplateError> {
        let recipes = builtin_recipes()?;
        let reg = hk_blocks::Registry::builtin();
        let mut lib = Self::default();
        for (name, text) in BUILTIN_TEMPLATES {
            lib.add_text(name, text, &recipes, &reg)?;
        }
        Ok(lib)
    }

    /// Adds every `*.json` template document in `dir` (a user data dir's `templates/`, or a test
    /// dir). A missing directory is an empty library, not an error. Recipes resolve against
    /// `recipes`.
    pub fn load_dir(
        &mut self,
        dir: &Path,
        recipes: &[Recipe],
        catalogue: &dyn Catalogue,
    ) -> Result<usize, TemplateError> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Ok(0);
        };
        let mut paths: Vec<_> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        let mut n = 0;
        for p in paths {
            let origin = p.display().to_string();
            let text = std::fs::read_to_string(&p)
                .map_err(|e| err(&origin, TemplateErrorCode::Parse, e.to_string()))?;
            self.add_text(&origin, &text, recipes, catalogue)?;
            n += 1;
        }
        Ok(n)
    }

    /// Parses, validates and adds one document.
    pub fn add_text(
        &mut self,
        origin: &str,
        text: &str,
        recipes: &[Recipe],
        catalogue: &dyn Catalogue,
    ) -> Result<(), TemplateError> {
        let t: Template = serde_json::from_str(text)
            .map_err(|e| err(origin, TemplateErrorCode::Parse, e.to_string()))?;
        let missing_blocks = validate(origin, &t, recipes, catalogue)?;
        let key = t.key();
        if self.entries.contains_key(&key) {
            return Err(err(origin, TemplateErrorCode::Duplicate, key));
        }
        self.entries.insert(
            key,
            LoadedTemplate {
                template: t,
                missing_blocks,
            },
        );
        Ok(())
    }

    /// Every template, by `id@version`.
    pub fn iter(&self) -> impl Iterator<Item = &LoadedTemplate> {
        self.entries.values()
    }

    /// One template by `id@version`.
    pub fn get(&self, key: &str) -> Option<&LoadedTemplate> {
        self.entries.get(key)
    }

    /// Inert templates with the blocks they lack (`missing_block`, `suspected_by: template`).
    pub fn inert(&self) -> impl Iterator<Item = (&str, &[String])> {
        self.entries
            .iter()
            .filter(|(_, l)| l.is_inert())
            .map(|(k, l)| (k.as_str(), l.missing_blocks.as_slice()))
    }

    /// The skeleton a generic, non-inert template offers.
    pub fn skeleton(&self, key: &str) -> Option<Skeleton> {
        self.get(key)
            .filter(|l| !l.is_inert())
            .and_then(|l| l.template.as_skeleton())
    }

    /// The families a template speaks for: its prior families, plus the `family` of any S1
    /// alternative of a skeleton.
    fn families(t: &Template) -> BTreeSet<String> {
        let mut f: BTreeSet<String> = t.priors.families.keys().cloned().collect();
        if let Some(s) = &t.skeleton {
            f.extend(
                s.slots
                    .get(&Stage::S1)
                    .into_iter()
                    .flatten()
                    .filter_map(|a| a.family.clone()),
            );
        }
        f
    }

    /// `match(h | measured)`: 1 when every measured prior fits or is unmeasured, times
    /// [`MISMATCH_FACTOR`] for each one contradicted. Never 0.
    fn match_factor(t: &Template, m: &MeasuredParams) -> f64 {
        let p = &t.priors;
        let mut f = 1.0;
        if let Some(r) = m.symbol_rate_bd
            && !p.symbol_rate_bd.is_empty()
            && !p.symbol_rate_bd.iter().any(|x| (x[0]..=x[1]).contains(&r))
        {
            f *= MISMATCH_FACTOR;
        }
        if let (Some(b), Some(x)) = (m.bandwidth_hz, p.bandwidth_hz)
            && !(x[0]..=x[1]).contains(&b)
        {
            f *= MISMATCH_FACTOR;
        }
        if let (Some(b), Some(x)) = (m.bursty, p.bursty)
            && b != x
        {
            f *= MISMATCH_FACTOR;
        }
        f
    }

    /// `band_factor`: [`BAND_FACTOR_MAX`] iff an emitter already detected at `centre_hz` lies in a
    /// declared band, else 1. Rank only.
    fn band_factor(t: &Template, m: &MeasuredParams) -> f64 {
        match m.centre_hz {
            Some(c) if t.priors.bands_hz.iter().any(|b| (b[0]..=b[1]).contains(&c)) => {
                BAND_FACTOR_MAX
            }
            _ => 1.0,
        }
    }

    /// Seeds a search from ADR-0016's ordered hypotheses (§4.2). Pure: reads the hint, the library
    /// and the measurement; writes nothing.
    ///
    /// Order: `full`-signature fast path first, then active families by `prior_bits`, then
    /// deferred ones (kept, not dropped). A template is offered under **every** family it speaks
    /// for that the seed listed. Inert templates are never offered. Generic skeletons are
    /// additionally always listed in [`TemplateSeeding::open_skeletons`].
    pub fn seed(&self, hint: &BudgetHint, measured: &MeasuredParams) -> TemplateSeeding {
        let mut ordered = Vec::new();
        for h in &hint.families_ordered {
            for l in self.entries.values().filter(|l| !l.is_inert()) {
                let t = &l.template;
                if !Self::families(t).contains(&h.family) {
                    continue;
                }
                let weight = t
                    .priors
                    .families
                    .get(&h.family)
                    .copied()
                    .unwrap_or(1.0)
                    .max(1e-6);
                let pi = h.posterior
                    * weight
                    * Self::match_factor(t, measured)
                    * Self::band_factor(t, measured);
                ordered.push(TemplateSeed {
                    template: t.key(),
                    family: h.family.clone(),
                    seed_source: seed_source(h),
                    prior_bits: prior_bits(pi),
                    deferred: h.prune,
                    fast_path: fast_path(h, t),
                });
            }
        }
        // Stable sort: fast path, then active before deferred, then higher prior first; ties keep
        // the seed's own order.
        ordered.sort_by(|a, b| {
            (!a.fast_path, a.deferred)
                .cmp(&(!b.fast_path, b.deferred))
                .then(b.prior_bits.total_cmp(&a.prior_bits))
        });
        let open_skeletons = self
            .entries
            .values()
            .filter(|l| !l.is_inert() && l.template.skeleton.is_some())
            .map(|l| l.template.key())
            .collect();
        TemplateSeeding {
            ordered,
            open_skeletons,
            open_search_min_share: hint.open_search_min_share.max(0.2),
        }
    }
}

fn seed_source(h: &Hypothesis) -> SeedSource {
    if h.boosts.is_empty() {
        SeedSource::Classification
    } else {
        SeedSource::Signature
    }
}

fn fast_path(h: &Hypothesis, t: &Template) -> bool {
    h.boosts.contains(&SeedBoost::SignatureFull)
        && t.recipe
            .as_ref()
            .is_some_and(|r| h.recipes.iter().any(|x| x.id == r.id))
}

/// The shipped recipes, parsed.
pub fn builtin_recipes() -> Result<Vec<Recipe>, TemplateError> {
    BUILTIN_RECIPES
        .iter()
        .map(|s| {
            serde_json::from_str::<Recipe>(s)
                .map_err(|e| err("builtin recipe", TemplateErrorCode::Parse, e.to_string()))
        })
        .collect()
}
