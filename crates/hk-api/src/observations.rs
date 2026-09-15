//! Observation log routes (T-115, ADR-0012 §8): `/api/observations`,
//! `/api/observations/coverage`. Planned in `docs/api.md` "Attention and memory (planned)".
//!
//! Stub pre-added by T-113: answers nothing until T-115 lands.

use crate::control::{CtlRequest, CtlResponse};
use crate::http::ApiState;

pub(crate) fn route(_state: &ApiState, _req: &CtlRequest<'_>) -> Option<CtlResponse> {
    None
}
