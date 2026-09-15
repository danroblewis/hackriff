//! Baseline, site, candidate and weight routes (T-119, ADR-0012 §8): `/api/sites[...]`,
//! `/api/baselines[...]`, `/api/candidates`, `/api/attention/weights`. Shapes in `docs/api.md`
//! "Sites, baselines, candidates and score weights".
//!
//! hk-api does not link hk-context: the pipeline's `AttentionService` answers through the
//! [`AttentionControl`] trait (the `RecipeControl` pattern), adapted in hk-cli. This module parses
//! and validates requests (unknown fields and malformed values are 400) and audits mutations via
//! [`crate::control::dispatch`] as `site_select`, `site_update`, `baseline_refreeze`,
//! `weights_update`.

use hk_model::attention::baseline::{BaselineResolution, HourOfWeek};
use hk_model::attention::score::ScoreWeights;
use hk_model::ids::SiteId;
use serde_json::{Map, Value};

use crate::control::{
    Applied, CtlRequest, CtlResponse, Fail, dispatch, number, only, refuse_route,
};
use crate::http::ApiState;

/// Default `/api/candidates` limit.
pub const CANDIDATES_DEFAULT_LIMIT: usize = 100;
/// Maximum `/api/candidates` limit.
pub const CANDIDATES_MAX_LIMIT: usize = 1_000;

/// `PUT /api/sites/current` body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteSelectBody {
    /// Select a known site.
    pub id: Option<SiteId>,
    /// Select or create the site of this name.
    pub name: Option<String>,
    /// Centroid of a created site, degrees.
    pub lat_deg: Option<f64>,
    /// Centroid of a created site, degrees.
    pub lon_deg: Option<f64>,
    /// Radius of a created site, m.
    pub radius_m: Option<f64>,
    /// UTC offset of a created site, minutes.
    pub utc_offset_min: Option<i16>,
    /// Release the pin.
    pub release: bool,
}

/// One attention call, validated.
#[derive(Clone, Debug, PartialEq)]
pub enum AttentionCall {
    /// `GET /api/sites`.
    Sites,
    /// `GET /api/sites/current`.
    CurrentSite,
    /// `PUT /api/sites/current`.
    SelectSite(SiteSelectBody),
    /// `PUT /api/sites/{id}`: `name` `Some(None)` clears it.
    UpdateSite {
        /// Site.
        id: SiteId,
        /// New name.
        name: Option<Option<String>>,
        /// New UTC offset, minutes.
        utc_offset_min: Option<i16>,
    },
    /// `GET /api/baselines`.
    Baselines {
        /// Site (default current).
        site: Option<SiteId>,
    },
    /// `GET /api/baselines/slots`.
    Slots {
        /// Site (default current).
        site: Option<SiteId>,
        /// Lower edge, Hz.
        f_lo: f64,
        /// Upper edge, Hz.
        f_hi: f64,
        /// Hour-of-week slot (default: now at the site).
        slot: Option<HourOfWeek>,
        /// Pool resolution (default: the finest mature).
        resolution: Option<BaselineResolution>,
    },
    /// `POST /api/baselines/refreeze`.
    Refreeze {
        /// Site (default current).
        site: Option<SiteId>,
        /// Lower edge, Hz.
        f_lo: Option<f64>,
        /// Upper edge, Hz.
        f_hi: Option<f64>,
    },
    /// `GET /api/candidates`.
    Candidates {
        /// Lower edge, Hz.
        f_lo: Option<f64>,
        /// Upper edge, Hz.
        f_hi: Option<f64>,
        /// Most candidates.
        limit: usize,
    },
    /// `GET /api/attention/weights`.
    Weights,
    /// `PUT /api/attention/weights`.
    SetWeights {
        /// Weights (version ignored).
        weights: ScoreWeights,
        /// Audit author (token id).
        author: String,
    },
}

/// A successful answer: the body, plus old/new values for the audit of a mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct AttentionAnswer {
    /// Response body.
    pub body: Value,
    /// Audited old value.
    pub old: Value,
    /// Audited new value.
    pub new: Value,
}

impl AttentionAnswer {
    /// A read answer.
    pub fn read(body: Value) -> Self {
        Self {
            body,
            old: Value::Null,
            new: Value::Null,
        }
    }
}

/// A refused or failed call.
#[derive(Clone, Debug, PartialEq)]
pub struct AttentionFail {
    /// HTTP status.
    pub status: u16,
    /// Stable code.
    pub code: &'static str,
    /// Message.
    pub message: String,
}

/// The pipeline side of the attention routes.
pub trait AttentionControl: Send + Sync {
    /// Runs one call.
    fn call(&self, call: AttentionCall) -> Result<AttentionAnswer, AttentionFail>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Route {
    Sites,
    Current,
    SelectSite,
    UpdateSite(SiteId),
    Baselines,
    Slots,
    Refreeze,
    Candidates,
    Weights,
    SetWeights,
}

impl Route {
    fn name(self) -> &'static str {
        match self {
            Self::Sites => "sites_list",
            Self::Current => "site_current",
            Self::SelectSite => "site_select",
            Self::UpdateSite(_) => "site_update",
            Self::Baselines => "baselines_list",
            Self::Slots => "baseline_slots",
            Self::Refreeze => "baseline_refreeze",
            Self::Candidates => "candidates",
            Self::Weights => "weights_get",
            Self::SetWeights => "weights_update",
        }
    }

    fn mutating(self) -> bool {
        matches!(
            self,
            Self::SelectSite | Self::UpdateSite(_) | Self::Refreeze | Self::SetWeights
        )
    }
}

fn resolve(method: &str, path: &str) -> Option<Result<Route, Option<&'static str>>> {
    let one = |m: &'static str, r: Route| if method == m { Ok(r) } else { Err(Some(m)) };
    Some(match path {
        "/api/sites" => one("GET", Route::Sites),
        "/api/sites/current" => match method {
            "GET" => Ok(Route::Current),
            "PUT" => Ok(Route::SelectSite),
            _ => Err(Some("GET, PUT")),
        },
        "/api/baselines" => one("GET", Route::Baselines),
        "/api/baselines/slots" => one("GET", Route::Slots),
        "/api/baselines/refreeze" => one("POST", Route::Refreeze),
        "/api/candidates" => one("GET", Route::Candidates),
        "/api/attention/weights" => match method {
            "GET" => Ok(Route::Weights),
            "PUT" => Ok(Route::SetWeights),
            _ => Err(Some("GET, PUT")),
        },
        _ => {
            let id = path.strip_prefix("/api/sites/")?;
            match id.parse::<SiteId>() {
                Ok(id) => one("PUT", Route::UpdateSite(id)),
                Err(_) => Err(None),
            }
        }
    })
}

/// Routes an attention request; `None` when `path` is not one.
pub(crate) fn route(state: &ApiState, req: &CtlRequest<'_>) -> Option<CtlResponse> {
    let route = match resolve(req.method, req.path)? {
        Ok(r) => r,
        Err(allow) => return Some(refuse_route(state, req, allow)),
    };
    let author = req
        .caller
        .token_id
        .clone()
        .unwrap_or_else(|| "local".into());
    Some(dispatch(
        state,
        req,
        route.name(),
        route.mutating(),
        |s| {
            let call = read_call(route, req.query)?;
            run(s, call).map(|a| a.body)
        },
        |s, body| {
            let call = write_call(route, body, author)?;
            let a = run(s, call)?;
            Ok(Applied {
                status: 200,
                body: a.body,
                old: a.old,
                new: a.new,
            })
        },
    ))
}

fn run(state: &ApiState, call: AttentionCall) -> Result<AttentionAnswer, Fail> {
    let ctl = state.attention.as_ref().ok_or_else(|| {
        Fail::new(
            503,
            "unavailable",
            "no attention service (baselines, sites, candidates) on this server",
        )
    })?;
    ctl.call(call)
        .map_err(|f| Fail::new(f.status, f.code, f.message))
}

fn query<'a>(q: &'a [(String, String)], key: &str) -> Option<&'a str> {
    q.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

fn query_only(q: &[(String, String)], allowed: &[&str]) -> Result<(), Fail> {
    match q.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(Fail::invalid(format!("unknown query parameter {k:?}"))),
        None => Ok(()),
    }
}

fn query_f64(q: &[(String, String)], key: &str) -> Result<Option<f64>, Fail> {
    query(q, key)
        .map(|v| {
            v.parse::<f64>()
                .ok()
                .filter(|x| x.is_finite())
                .ok_or_else(|| Fail::invalid(format!("{key} must be a finite number")))
        })
        .transpose()
}

fn query_site(q: &[(String, String)]) -> Result<Option<SiteId>, Fail> {
    query(q, "site")
        .map(|v| {
            v.parse::<SiteId>()
                .map_err(|_| Fail::invalid("site must be a site id"))
        })
        .transpose()
}

fn band(f_lo: Option<f64>, f_hi: Option<f64>) -> Result<(), Fail> {
    match (f_lo, f_hi) {
        (Some(lo), Some(hi)) if hi <= lo => Err(Fail::invalid("f_hi must exceed f_lo")),
        _ => Ok(()),
    }
}

fn read_call(route: Route, q: &[(String, String)]) -> Result<AttentionCall, Fail> {
    Ok(match route {
        Route::Sites => {
            query_only(q, &[])?;
            AttentionCall::Sites
        }
        Route::Current => {
            query_only(q, &[])?;
            AttentionCall::CurrentSite
        }
        Route::Baselines => {
            query_only(q, &["site"])?;
            AttentionCall::Baselines {
                site: query_site(q)?,
            }
        }
        Route::Slots => {
            query_only(q, &["site", "f_lo", "f_hi", "slot", "resolution"])?;
            let (f_lo, f_hi) = (query_f64(q, "f_lo")?, query_f64(q, "f_hi")?);
            let (Some(f_lo), Some(f_hi)) = (f_lo, f_hi) else {
                return Err(Fail::invalid("f_lo and f_hi are required"));
            };
            band(Some(f_lo), Some(f_hi))?;
            let slot = query(q, "slot")
                .map(|v| {
                    v.parse::<u8>()
                        .ok()
                        .and_then(|s| HourOfWeek::try_from(s).ok())
                        .ok_or_else(|| Fail::invalid("slot must be 0–167"))
                })
                .transpose()?;
            let resolution = query(q, "resolution")
                .map(|v| {
                    serde_json::from_value::<BaselineResolution>(Value::String(v.into()))
                        .map_err(|_| {
                            Fail::invalid(
                                "resolution must be hour-of-week, hour-of-day, day-part or all-hours",
                            )
                        })
                })
                .transpose()?;
            AttentionCall::Slots {
                site: query_site(q)?,
                f_lo,
                f_hi,
                slot,
                resolution,
            }
        }
        Route::Candidates => {
            query_only(q, &["f_lo", "f_hi", "limit"])?;
            let (f_lo, f_hi) = (query_f64(q, "f_lo")?, query_f64(q, "f_hi")?);
            band(f_lo, f_hi)?;
            let limit = query(q, "limit")
                .map(|v| {
                    v.parse::<usize>()
                        .ok()
                        .filter(|l| (1..=CANDIDATES_MAX_LIMIT).contains(l))
                        .ok_or_else(|| Fail::invalid("limit must be 1–1000"))
                })
                .transpose()?
                .unwrap_or(CANDIDATES_DEFAULT_LIMIT);
            AttentionCall::Candidates { f_lo, f_hi, limit }
        }
        Route::Weights => {
            query_only(q, &[])?;
            AttentionCall::Weights
        }
        _ => return Err(Fail::new(500, "failed", "not a read")),
    })
}

fn opt_text(body: &Map<String, Value>, key: &str) -> Result<Option<Option<String>>, Fail> {
    match body.get(key) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(None)),
        Some(Value::String(s)) if !s.trim().is_empty() && s.trim().len() <= 64 => {
            Ok(Some(Some(s.trim().to_owned())))
        }
        Some(_) => Err(Fail::invalid(format!(
            "{key} must be a non-blank string of at most 64 bytes"
        ))),
    }
}

fn offset(body: &Map<String, Value>) -> Result<Option<i16>, Fail> {
    number(body, "utc_offset_min")?
        .map(|v| {
            if v.fract() == 0.0 && (-840.0..=840.0).contains(&v) {
                Ok(v as i16)
            } else {
                Err(Fail::invalid(
                    "utc_offset_min must be an integer within ±840",
                ))
            }
        })
        .transpose()
}

fn write_call(
    route: Route,
    body: &Map<String, Value>,
    author: String,
) -> Result<AttentionCall, Fail> {
    Ok(match route {
        Route::SelectSite => {
            only(
                body,
                &[
                    "id",
                    "name",
                    "lat_deg",
                    "lon_deg",
                    "radius_m",
                    "utc_offset_min",
                    "release",
                ],
            )?;
            let id = match body.get("id") {
                None => None,
                Some(Value::String(s)) => Some(
                    s.parse::<SiteId>()
                        .map_err(|_| Fail::invalid("id must be a site id"))?,
                ),
                Some(_) => return Err(Fail::invalid("id must be a site id")),
            };
            let release = match body.get("release") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(Fail::invalid("release must be a boolean")),
            };
            let name = opt_text(body, "name")?.flatten();
            if release == (id.is_some() || name.is_some()) {
                return Err(Fail::invalid(
                    "give exactly one of id or name, or release: true",
                ));
            }
            if id.is_some() && name.is_some() {
                return Err(Fail::invalid("give id or name, not both"));
            }
            AttentionCall::SelectSite(SiteSelectBody {
                id,
                name,
                lat_deg: number(body, "lat_deg")?,
                lon_deg: number(body, "lon_deg")?,
                radius_m: number(body, "radius_m")?,
                utc_offset_min: offset(body)?,
                release,
            })
        }
        Route::UpdateSite(id) => {
            only(body, &["name", "utc_offset_min"])?;
            let name = opt_text(body, "name")?;
            let utc_offset_min = offset(body)?;
            if name.is_none() && utc_offset_min.is_none() {
                return Err(Fail::invalid("give name and/or utc_offset_min"));
            }
            AttentionCall::UpdateSite {
                id,
                name,
                utc_offset_min,
            }
        }
        Route::Refreeze => {
            only(body, &["site", "f_lo", "f_hi"])?;
            let site = match body.get("site") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => Some(
                    s.parse::<SiteId>()
                        .map_err(|_| Fail::invalid("site must be a site id"))?,
                ),
                Some(_) => return Err(Fail::invalid("site must be a site id")),
            };
            let (f_lo, f_hi) = (number(body, "f_lo")?, number(body, "f_hi")?);
            if f_lo.is_some() != f_hi.is_some() {
                return Err(Fail::invalid("give both f_lo and f_hi, or neither"));
            }
            band(f_lo, f_hi)?;
            AttentionCall::Refreeze { site, f_lo, f_hi }
        }
        Route::SetWeights => {
            const FIELDS: [&str; 6] = [
                "snr",
                "novelty",
                "class_entropy",
                "decoder",
                "periodicity",
                "boring",
            ];
            only(body, &FIELDS)?;
            let mut v = [0.0; 6];
            for (slot, key) in v.iter_mut().zip(FIELDS) {
                *slot = number(body, key)?
                    .ok_or_else(|| Fail::invalid(format!("{key} is required")))?;
            }
            let weights = ScoreWeights {
                version: 1,
                snr: v[0],
                novelty: v[1],
                class_entropy: v[2],
                decoder: v[3],
                periodicity: v[4],
                boring: v[5],
            };
            weights
                .validate()
                .map_err(|e| Fail::invalid(e.to_string()))?;
            AttentionCall::SetWeights { weights, author }
        }
        _ => return Err(Fail::new(500, "failed", "not a mutation")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_routes_resolve_with_methods() {
        assert_eq!(resolve("GET", "/api/sites"), Some(Ok(Route::Sites)));
        assert_eq!(resolve("POST", "/api/sites"), Some(Err(Some("GET"))));
        assert_eq!(
            resolve("PUT", "/api/sites/current"),
            Some(Ok(Route::SelectSite))
        );
        let id = SiteId::new();
        assert_eq!(
            resolve("PUT", &format!("/api/sites/{id}")),
            Some(Ok(Route::UpdateSite(id)))
        );
        assert_eq!(resolve("PUT", "/api/sites/nope"), Some(Err(None)));
        assert_eq!(
            resolve("DELETE", "/api/attention/weights"),
            Some(Err(Some("GET, PUT")))
        );
        assert_eq!(
            resolve("POST", "/api/baselines/refreeze"),
            Some(Ok(Route::Refreeze))
        );
        assert_eq!(resolve("GET", "/api/other"), None);
    }

    #[test]
    fn attention_request_validation() {
        let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert!(read_call(Route::Slots, &q(&[("f_lo", "1")])).is_err());
        assert!(read_call(Route::Slots, &q(&[("f_lo", "2"), ("f_hi", "1")])).is_err());
        assert!(
            read_call(
                Route::Slots,
                &q(&[("f_lo", "1"), ("f_hi", "2"), ("resolution", "weekly")])
            )
            .is_err()
        );
        assert_eq!(
            read_call(
                Route::Slots,
                &q(&[
                    ("f_lo", "1"),
                    ("f_hi", "2"),
                    ("slot", "5"),
                    ("resolution", "day-part")
                ])
            )
            .ok(),
            Some(AttentionCall::Slots {
                site: None,
                f_lo: 1.0,
                f_hi: 2.0,
                slot: Some(HourOfWeek::try_from(5).unwrap()),
                resolution: Some(BaselineResolution::DayPart),
            })
        );
        assert!(read_call(Route::Candidates, &q(&[("limit", "0")])).is_err());
        assert!(read_call(Route::Candidates, &q(&[("x", "0")])).is_err());
        let body = |v: Value| v.as_object().unwrap().clone();
        let w = serde_json::json!({"snr": 1, "novelty": 2, "class_entropy": 1, "decoder": 0.5,
            "periodicity": 0.5, "boring": 1});
        assert!(matches!(
            write_call(Route::SetWeights, &body(w.clone()), "t".into()),
            Ok(AttentionCall::SetWeights { .. })
        ));
        let mut bad = w.clone();
        bad["snr"] = serde_json::json!(11);
        assert!(write_call(Route::SetWeights, &body(bad), "t".into()).is_err());
        let mut missing = w;
        missing.as_object_mut().unwrap().remove("boring");
        assert!(write_call(Route::SetWeights, &body(missing), "t".into()).is_err());
        let sel = |v| write_call(Route::SelectSite, &body(v), "t".into());
        assert!(sel(serde_json::json!({})).is_err());
        assert!(sel(serde_json::json!({"name": "home", "release": true})).is_err());
        assert!(sel(serde_json::json!({"name": "home", "utc_offset_min": 30})).is_ok());
        assert!(sel(serde_json::json!({"name": "home", "utc_offset_min": 1.5})).is_err());
        assert!(sel(serde_json::json!({"release": true})).is_ok());
        assert!(
            write_call(
                Route::Refreeze,
                &body(serde_json::json!({"f_lo": 1})),
                "t".into()
            )
            .is_err()
        );
    }
}
