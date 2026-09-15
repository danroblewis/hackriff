//! Decoded-capture routes (`GET /api/captures`, `/api/captures/{id}/frames`) (T-092, ADR-0011 §7). Stub pre-added by T-085 so T-092 fills in only this file:
//! the `pub mod` line, the dispatch in `http.rs` and the `hk-recipe` dependency already exist.
//! Answers nothing until then.

use crate::control::{CtlRequest, CtlResponse};
use crate::http::ApiState;

/// This module's routes; `None` = not mine.
pub(crate) fn route(_state: &ApiState, _req: &CtlRequest<'_>) -> Option<CtlResponse> {
    None
}
