//! Baseline store (T-119, ADR-0012 §3.6, §9): one file per `BaselineKey` holding the frozen
//! reference and adaptive slot statistics, rewritten temp → fsync → rename at slot close, with a
//! byte quota evicting the least recently visited sites.
//!
//! Stub pre-added by T-113.
