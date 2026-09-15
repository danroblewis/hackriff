//! Observation records from applied steps (T-115, ADR-0012 §1): maps a [`super::ScheduleStep`]
//! plus what was actually applied and analysed into `hk_model::attention::observation` records,
//! using [`super::Purpose::reason`]. Pure mapping; the pipeline owns the sink.
//!
//! Stub pre-added by T-113.
