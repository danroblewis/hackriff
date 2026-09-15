//! Survey report routes (T-121, ADR-0012 §8): `/api/report`. Planned in `docs/api.md`
//! "Attention and memory (planned)".
//!
//! Stub pre-added by T-113: answers nothing until T-121 lands.

use crate::control::{CtlRequest, CtlResponse};
use crate::http::ApiState;

pub(crate) fn route(_state: &ApiState, _req: &CtlRequest<'_>) -> Option<CtlResponse> {
    None
}
