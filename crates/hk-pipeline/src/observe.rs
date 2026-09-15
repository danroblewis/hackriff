//! Observation log wiring (T-115, ADR-0012 §1): collects applied steps and analysed extents from
//! the control thread and hands records to `hk_store::observation` on a bounded queue that never
//! blocks the pipeline. T-115 also owns the single observer call site in `control.rs`.
//!
//! Stub pre-added by T-113.
