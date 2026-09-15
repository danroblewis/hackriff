//! Modulation taxonomy as data (ADR-0016 §1). **Core interface.**
//!
//! A taxonomy is a tree: coarse (analog / digital / noise-like) → family → class. `unknown` is the
//! global open-set outcome at every level, never a leaf. Class names reuse the labels the pipeline
//! already writes (`2fsk`, `wfm`, `bpsk` …).
//!
//! **Versioning.** Adding a class or a family is a new version (`hk-mod@2`); a version, once
//! released, never changes. Stored rows keep the version they were written under, and readers map
//! labels through [`family_of`] with that version. Pre-M3 labels (`fsk`, `ook`, `bpsk`, `qpsk`,
//! `wfm`, `nbfm`, `am`, `2fsk`, …) map into `hk-mod@1`. Service families (`adsb`, `fm-broadcast`,
//! decoder ids) are not modulation labels: they map to nothing here and stay decoder/occupancy
//! evidence in `hk-pipeline::family`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The open-set outcome label, valid at every level of every taxonomy.
pub const UNKNOWN: &str = "unknown";

/// Coarse level of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Coarse {
    /// Analog modulation (AM, FM, SSB, CW).
    Analog,
    /// Digital modulation.
    Digital,
    /// Noise-like emission: flat PSD, spectral kurtosis ≈ 0, no cyclic line.
    NoiseLike,
    /// Open-set outcome: no coarse call.
    Unknown,
}

impl Coarse {
    /// The serde/label string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Coarse::Analog => "analog",
            Coarse::Digital => "digital",
            Coarse::NoiseLike => "noise-like",
            Coarse::Unknown => UNKNOWN,
        }
    }
}

/// One family of a taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FamilyDef {
    /// Family label, e.g. `fsk`.
    pub name: &'static str,
    /// Its coarse branch (never [`Coarse::Unknown`]).
    pub coarse: Coarse,
    /// Class labels within the family, e.g. `2fsk`, `gfsk`.
    pub classes: &'static [&'static str],
}

/// A versioned taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taxonomy {
    /// Name, e.g. `hk-mod`.
    pub name: &'static str,
    /// Version (≥ 1).
    pub version: u32,
    /// Families in display order.
    pub families: &'static [FamilyDef],
    /// Pre-taxonomy labels written before this version existed, mapped to a family of this
    /// version: `(legacy label, family)`. Labels that are already family or class names are not
    /// repeated here.
    pub legacy: &'static [(&'static str, &'static str)],
}

/// `hk-mod@1` (ADR-0016 §1).
pub const HK_MOD_V1: Taxonomy = Taxonomy {
    name: "hk-mod",
    version: 1,
    families: &[
        FamilyDef {
            name: "analog",
            coarse: Coarse::Analog,
            classes: &["am", "nbfm", "wfm", "ssb", "cw"],
        },
        FamilyDef {
            name: "ook-ask",
            coarse: Coarse::Digital,
            classes: &["ook", "ask4"],
        },
        FamilyDef {
            name: "fsk",
            coarse: Coarse::Digital,
            classes: &["2fsk", "gfsk", "msk", "4fsk"],
        },
        FamilyDef {
            name: "psk-qam",
            coarse: Coarse::Digital,
            classes: &["bpsk", "qpsk", "8psk", "qam16", "qam64"],
        },
        FamilyDef {
            name: "ofdm",
            coarse: Coarse::Digital,
            classes: &["ofdm"],
        },
        FamilyDef {
            name: "css",
            coarse: Coarse::Digital,
            classes: &["chirp"],
        },
        FamilyDef {
            name: "dsss",
            coarse: Coarse::Digital,
            classes: &["dsss"],
        },
        FamilyDef {
            name: "pulsed",
            coarse: Coarse::Digital,
            classes: &["ppm", "pulse"],
        },
        FamilyDef {
            name: "noise-like",
            coarse: Coarse::NoiseLike,
            classes: &["noise-like"],
        },
    ],
    // `blind::Family` (`ook`/`fsk`/`bpsk`/`qpsk`) and the analog/FSK chain labels are already
    // family or class names; `ask` and `fsk2` are the spellings older docs and tests used.
    legacy: &[("ask", "ook-ask"), ("fsk2", "fsk")],
};

/// Every released taxonomy, oldest first.
pub const TAXONOMIES: &[Taxonomy] = &[HK_MOD_V1];

/// The taxonomy new classifications are written under.
pub const CURRENT: &Taxonomy = &HK_MOD_V1;

/// A taxonomy reference, `<name>@<version>` (e.g. `hk-mod@1`). Serialises as that string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TaxonomyRef {
    /// Name.
    pub name: String,
    /// Version.
    pub version: u32,
}

impl TaxonomyRef {
    /// The reference of `t`.
    pub fn of(t: &Taxonomy) -> Self {
        Self {
            name: t.name.to_owned(),
            version: t.version,
        }
    }

    /// The reference of [`CURRENT`].
    pub fn current() -> Self {
        Self::of(CURRENT)
    }

    /// The released taxonomy it names, if any.
    pub fn resolve(&self) -> Option<&'static Taxonomy> {
        lookup(&self.name, self.version)
    }
}

impl fmt::Display for TaxonomyRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

impl FromStr for TaxonomyRef {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (name, version) = s
            .split_once('@')
            .ok_or_else(|| format!("taxonomy ref {s:?} is not <name>@<version>"))?;
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        let version: u32 = version
            .parse()
            .ok()
            .filter(|v| *v >= 1 && !version.starts_with('0'))
            .ok_or_else(|| format!("taxonomy ref {s:?} has an invalid version"))?;
        if !valid_name {
            return Err(format!("taxonomy ref {s:?} has an invalid name"));
        }
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }
}

impl Serialize for TaxonomyRef {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TaxonomyRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The released taxonomy `name@version`.
pub fn lookup(name: &str, version: u32) -> Option<&'static Taxonomy> {
    TAXONOMIES
        .iter()
        .find(|t| t.name == name && t.version == version)
}

impl Taxonomy {
    /// The family named `name` (not a class or legacy label).
    pub fn family(&self, name: &str) -> Option<&'static FamilyDef> {
        self.families.iter().find(|f| f.name == name)
    }

    /// Whether `label` is a family of this taxonomy.
    pub fn is_family(&self, label: &str) -> bool {
        self.families.iter().any(|f| f.name == label)
    }

    /// The family a class label belongs to.
    pub fn family_of_class(&self, class: &str) -> Option<&'static str> {
        self.families
            .iter()
            .find(|f| f.classes.contains(&class))
            .map(|f| f.name)
    }

    /// The family of `label` (a family, class or legacy label, trimmed, ASCII case-insensitive):
    /// `Some("unknown")` for `unknown`, `None` for labels outside the taxonomy (service families,
    /// decoder ids).
    pub fn family_of(&self, label: &str) -> Option<&'static str> {
        let l = label.trim().to_ascii_lowercase();
        if l == UNKNOWN {
            return Some(UNKNOWN);
        }
        if let Some(f) = self.families.iter().find(|f| f.name == l) {
            return Some(f.name);
        }
        if let Some(f) = self.family_of_class(&l) {
            return Some(f);
        }
        self.legacy
            .iter()
            .find(|(legacy, _)| *legacy == l)
            .and_then(|(_, fam)| self.family(fam))
            .map(|f| f.name)
    }

    /// The coarse branch of `label` (via [`Self::family_of`]); [`Coarse::Unknown`] for `unknown`.
    pub fn coarse_of(&self, label: &str) -> Option<Coarse> {
        match self.family_of(label)? {
            UNKNOWN => Some(Coarse::Unknown),
            fam => self.family(fam).map(|f| f.coarse),
        }
    }

    /// Structural validation: name/version well formed, family and class labels non-empty,
    /// lowercase and unique across the tree (a class may share its own family's name, e.g.
    /// `ofdm`), no family on the `unknown` branch, no label spelled `unknown`, legacy labels
    /// unused elsewhere and pointing at families.
    pub fn validate(&self) -> Result<(), String> {
        let label_ok = |l: &str| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        format!("{}@{}", self.name, self.version).parse::<TaxonomyRef>()?;
        let mut seen: Vec<(&str, &str)> = Vec::new(); // (label, owning family)
        for f in self.families {
            if !label_ok(f.name) || f.name == UNKNOWN {
                return Err(format!("invalid family label {:?}", f.name));
            }
            if f.coarse == Coarse::Unknown {
                return Err(format!("family {} is on the unknown branch", f.name));
            }
            if f.classes.is_empty() {
                return Err(format!("family {} has no classes", f.name));
            }
            if seen.iter().any(|(l, _)| *l == f.name) {
                return Err(format!("label {} is not unique", f.name));
            }
            seen.push((f.name, f.name));
        }
        for f in self.families {
            for c in f.classes {
                if !label_ok(c) || *c == UNKNOWN {
                    return Err(format!("invalid class label {c:?}"));
                }
                let clash = seen
                    .iter()
                    .any(|(l, owner)| l == c && !(*l == f.name && *owner == f.name));
                if clash {
                    return Err(format!("label {c} is not unique"));
                }
                seen.push((c, f.name));
            }
        }
        for (legacy, fam) in self.legacy {
            if !label_ok(legacy) || seen.iter().any(|(l, _)| l == legacy) {
                return Err(format!(
                    "legacy label {legacy:?} is invalid or shadows a label"
                ));
            }
            if !self.is_family(fam) {
                return Err(format!("legacy label {legacy} maps to non-family {fam}"));
            }
        }
        Ok(())
    }
}

/// [`Taxonomy::family_of`] under the released taxonomy `name@version`; `None` when the version is
/// not released or the label is outside it.
pub fn family_of(label: &str, taxonomy: &TaxonomyRef) -> Option<&'static str> {
    taxonomy.resolve()?.family_of(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_taxonomies_are_valid_and_unique() {
        for t in TAXONOMIES {
            t.validate()
                .unwrap_or_else(|e| panic!("{}@{}: {e}", t.name, t.version));
        }
        for (i, a) in TAXONOMIES.iter().enumerate() {
            for b in &TAXONOMIES[i + 1..] {
                assert!(a.name != b.name || a.version != b.version);
            }
        }
        assert_eq!(TaxonomyRef::current().to_string(), "hk-mod@1");
    }

    #[test]
    fn hk_mod_v1_has_the_adr_tree() {
        let t = &HK_MOD_V1;
        let families: Vec<_> = t.families.iter().map(|f| f.name).collect();
        assert_eq!(
            families,
            [
                "analog",
                "ook-ask",
                "fsk",
                "psk-qam",
                "ofdm",
                "css",
                "dsss",
                "pulsed",
                "noise-like"
            ]
        );
        assert_eq!(t.coarse_of("analog"), Some(Coarse::Analog));
        assert_eq!(t.coarse_of("pulsed"), Some(Coarse::Digital));
        assert_eq!(t.coarse_of("noise-like"), Some(Coarse::NoiseLike));
        assert_eq!(t.coarse_of(UNKNOWN), Some(Coarse::Unknown));
        assert_eq!(t.family_of_class("qam64"), Some("psk-qam"));
        assert_eq!(t.family_of_class("fsk"), None, "a family is not a class");
    }

    #[test]
    fn pre_m3_labels_map_into_hk_mod_v1() {
        let v1 = TaxonomyRef::current();
        for (label, family) in [
            ("fsk", "fsk"),
            ("2fsk", "fsk"),
            ("fsk2", "fsk"),
            ("ook", "ook-ask"),
            ("ask", "ook-ask"),
            ("bpsk", "psk-qam"),
            ("qpsk", "psk-qam"),
            ("wfm", "analog"),
            ("nbfm", "analog"),
            ("am", "analog"),
            ("ssb", "analog"),
            ("cw", "analog"),
            (" WFM ", "analog"),
            ("unknown", "unknown"),
        ] {
            assert_eq!(family_of(label, &v1), Some(family), "{label}");
        }
        for service in ["adsb", "fm-broadcast", "readsb", "rds", "", "fsk-2"] {
            assert_eq!(family_of(service, &v1), None, "{service}");
        }
        let unreleased: TaxonomyRef = "hk-mod@2".parse().unwrap();
        assert_eq!(family_of("fsk", &unreleased), None);
    }

    #[test]
    fn taxonomy_ref_parses_and_round_trips() {
        let r: TaxonomyRef = "hk-mod@1".parse().unwrap();
        assert_eq!(r, TaxonomyRef::current());
        assert_eq!(serde_json::to_string(&r).unwrap(), "\"hk-mod@1\"");
        let back: TaxonomyRef = serde_json::from_str("\"hk-mod@1\"").unwrap();
        assert_eq!(back, r);
        for bad in [
            "hk-mod",
            "hk-mod@",
            "hk-mod@0",
            "hk-mod@01",
            "@1",
            "HK@1",
            "a b@1",
        ] {
            assert!(bad.parse::<TaxonomyRef>().is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<TaxonomyRef>("\"nope\"").is_err());
    }

    #[test]
    fn validation_rejects_malformed_trees() {
        const DUP: Taxonomy = Taxonomy {
            name: "t",
            version: 1,
            families: &[
                FamilyDef {
                    name: "a",
                    coarse: Coarse::Digital,
                    classes: &["x"],
                },
                FamilyDef {
                    name: "b",
                    coarse: Coarse::Digital,
                    classes: &["x"],
                },
            ],
            legacy: &[],
        };
        assert!(DUP.validate().is_err());
        const UNK: Taxonomy = Taxonomy {
            name: "t",
            version: 1,
            families: &[FamilyDef {
                name: "a",
                coarse: Coarse::Digital,
                classes: &["unknown"],
            }],
            legacy: &[],
        };
        assert!(UNK.validate().is_err());
        const SHADOW: Taxonomy = Taxonomy {
            name: "t",
            version: 1,
            families: &[FamilyDef {
                name: "a",
                coarse: Coarse::Analog,
                classes: &["x"],
            }],
            legacy: &[("x", "a")],
        };
        assert!(SHADOW.validate().is_err());
        const UNKNOWN_BRANCH: Taxonomy = Taxonomy {
            name: "t",
            version: 1,
            families: &[FamilyDef {
                name: "a",
                coarse: Coarse::Unknown,
                classes: &["x"],
            }],
            legacy: &[],
        };
        assert!(UNKNOWN_BRANCH.validate().is_err());
    }
}
