//! The run's on-demand chain budget (T-071): one admission for every chain a consumer asks for,
//! Listen chains ([`super::listen`]) and burst taps ([`super::taps`]), replacing T-066's listen
//! budget and T-060's separate tap cap.
//!
//! - **Count:** at most `max_chains` on-demand chains together, and per kind `max_listeners` and
//!   `max_taps` (a per-kind limit larger than `max_chains` raises it).
//! - **CPU:** each chain is costed (Listen from its tuned rate and mode, a tap at `tap_cores`)
//!   against `cpu_fraction` of the cores. A chain is always admitted when nothing costed runs, so a
//!   small device can still listen at a high rate.
//! - Beyond either, the request is refused `503 busy` with the counts and cores in the reason;
//!   running chains are untouched (admission only reads and bumps counters).
//! - A [`Slot`] is released once, by whichever comes first: the session guard dropping (the
//!   client left) or the chain ending.
//!
//! `/api/status` reports `budget` ([`crate::stats::BudgetCounters::status_json`]); the T-066
//! `listen.budget` object keeps its listeners-only view.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use hk_stream::OpenRefusal;

use crate::stats::{Counters, get, inc};

/// What an on-demand chain is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainKind {
    /// A Listen audio chain.
    Listen,
    /// A bits or symbols burst tap.
    Tap,
    /// A decoder-workbench recipe pipeline (T-088, ADR-0011 §1.4 rule 5): counted in `chains`
    /// and the CPU budget only.
    Recipe,
}

/// The limits admission applies.
#[derive(Clone, Debug, PartialEq)]
pub struct BudgetLimits {
    /// Most on-demand chains together.
    pub max_chains: usize,
    /// Most Listen chains.
    pub max_listeners: usize,
    /// Most burst taps.
    pub max_taps: usize,
    /// CPU budget, millicores.
    pub budget_mcores: u64,
}

impl BudgetLimits {
    /// The count limit in force: `max_chains`, raised to a larger per-kind limit.
    pub fn effective_max_chains(&self) -> usize {
        self.max_chains.max(self.max_listeners).max(self.max_taps)
    }
}

/// Publishes `limits` into the run's counters (`/api/status`).
pub(crate) fn publish(counters: &Counters, limits: &BudgetLimits) {
    let b = &counters.budget;
    let s = |a: &AtomicU64, v: u64| a.store(v, Ordering::Relaxed);
    s(&b.limit_chains, limits.effective_max_chains() as u64);
    s(&b.limit_listeners, limits.max_listeners as u64);
    s(&b.limit_taps, limits.max_taps as u64);
    s(&b.budget_mcores, limits.budget_mcores);
    let lc = &counters.listen;
    s(&lc.limit_listeners, limits.max_listeners as u64);
    s(&lc.budget_mcores, limits.budget_mcores);
}

/// Cores to millicores (0 for non-finite or negative values).
pub(crate) fn mcores(cores: f64) -> u64 {
    if cores.is_finite() && cores > 0.0 {
        (cores * 1e3).round() as u64
    } else {
        0
    }
}

fn cores(mcores: u64) -> f64 {
    mcores as f64 / 1e3
}

struct SlotInner {
    counters: Arc<Counters>,
    kind: ChainKind,
    mcores: AtomicU64,
    released: AtomicBool,
}

fn sub(a: &AtomicU64, n: u64) {
    let _ = a.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
        Some(u.saturating_sub(n))
    });
}

impl SlotInner {
    fn release(&self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let _g = self
            .counters
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let m = self.mcores.load(Ordering::SeqCst);
        let b = &self.counters.budget;
        sub(&b.chains, 1);
        sub(&b.used_mcores, m);
        match self.kind {
            ChainKind::Listen => {
                sub(&b.listeners, 1);
                let lc = &self.counters.listen;
                sub(&lc.active, 1);
                sub(&lc.budget_used_mcores, m);
            }
            ChainKind::Tap => sub(&b.taps, 1),
            ChainKind::Recipe => {}
        }
    }
}

impl Drop for SlotInner {
    fn drop(&mut self) {
        self.release();
    }
}

/// One admitted chain's share of the budget.
#[derive(Clone)]
pub(crate) struct Slot(Arc<SlotInner>);

impl Slot {
    /// Admits one chain of `kind` estimated at `need` millicores (on a source at `rate_hz`, for
    /// the reason), or refuses with 503 `busy`.
    pub fn claim(
        counters: &Arc<Counters>,
        limits: &BudgetLimits,
        kind: ChainKind,
        need: u64,
        rate_hz: f64,
    ) -> Result<Self, OpenRefusal> {
        let _g = counters
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publish(counters, limits);
        let b = &counters.budget;
        let (chains, listeners, taps, used) = (
            get(&b.chains),
            get(&b.listeners),
            get(&b.taps),
            get(&b.used_mcores),
        );
        let max = limits.effective_max_chains() as u64;
        let (max_l, max_t) = (limits.max_listeners as u64, limits.max_taps as u64);
        let budget = limits.budget_mcores;
        let in_use = format!(
            "{:.2} of {:.2} CPU cores in use",
            cores(used),
            cores(budget)
        );
        let refusal = match kind {
            ChainKind::Listen if listeners >= max_l => Some(format!(
                "listener limit: {listeners} of {max_l} listeners running ({in_use}); stop one \
                 and try again"
            )),
            ChainKind::Tap if taps >= max_t => Some(format!(
                "burst tap limit: {taps} of {max_t} burst taps already open ({in_use}); close \
                 one and try again"
            )),
            _ if chains >= max => Some(format!(
                "chain budget: {chains} of {max} chains running ({listeners} listeners, {taps} \
                 taps; {in_use}); stop one and try again"
            )),
            _ if used > 0 && used + need > budget => Some(format!(
                "CPU budget: this chain needs about {:.2} cores at {:.2} Msps; {:.2} of {:.2} \
                 cores in use by {listeners} of {max_l} listeners and {taps} taps ({chains} of \
                 {max} chains); stop one and try again",
                cores(need),
                rate_hz / 1e6,
                cores(used),
                cores(budget)
            )),
            _ => None,
        };
        if let Some(reason) = refusal {
            inc(&b.refused_busy);
            return Err(OpenRefusal::new(503, "busy", reason));
        }
        let add = |a: &AtomicU64, n: u64| a.fetch_add(n, Ordering::SeqCst);
        add(&b.chains, 1);
        add(&b.used_mcores, need);
        match kind {
            ChainKind::Listen => {
                add(&b.listeners, 1);
                add(&counters.listen.active, 1);
                add(&counters.listen.budget_used_mcores, need);
            }
            ChainKind::Tap => {
                add(&b.taps, 1);
            }
            ChainKind::Recipe => {}
        }
        Ok(Self(Arc::new(SlotInner {
            counters: Arc::clone(counters),
            kind,
            mcores: AtomicU64::new(need),
            released: AtomicBool::new(false),
        })))
    }

    /// Re-costs the chain once its mode is known.
    pub fn set_mcores(&self, need: u64) {
        let s = &self.0;
        let _g = s
            .counters
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.released.load(Ordering::SeqCst) {
            return;
        }
        let old = s.mcores.swap(need, Ordering::SeqCst);
        let upd = |a: &AtomicU64| {
            let _ = a.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
                Some((u + need).saturating_sub(old))
            });
        };
        upd(&s.counters.budget.used_mcores);
        if s.kind == ChainKind::Listen {
            upd(&s.counters.listen.budget_used_mcores);
        }
    }

    /// Frees the slot (once).
    pub fn release(&self) {
        self.0.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> BudgetLimits {
        BudgetLimits {
            max_chains: 3,
            max_listeners: 2,
            max_taps: 2,
            budget_mcores: 1000,
        }
    }

    #[test]
    fn listeners_and_taps_share_one_count_and_cpu_budget() {
        let c = Arc::new(Counters::default());
        let l = limits();
        let a = Slot::claim(&c, &l, ChainKind::Listen, 400, 2.4e6).unwrap();
        let t = Slot::claim(&c, &l, ChainKind::Tap, 10, 0.0).unwrap();
        let e = Slot::claim(&c, &l, ChainKind::Listen, 700, 2.4e6)
            .err()
            .unwrap();
        assert!(e.reason.contains("CPU budget"), "{e}");
        assert!(e.reason.contains("1 taps"), "{e}");
        let b = Slot::claim(&c, &l, ChainKind::Listen, 400, 2.4e6).unwrap();
        // Three chains of three: a tap is refused by the shared count, naming it.
        let e = Slot::claim(&c, &l, ChainKind::Tap, 10, 0.0).err().unwrap();
        assert_eq!((e.status, e.code.as_str()), (503, "busy"));
        assert!(e.reason.contains("chain budget: 3 of 3 chains"), "{e}");
        let e = Slot::claim(&c, &l, ChainKind::Listen, 1, 2.4e6)
            .err()
            .unwrap();
        assert!(e.reason.contains("2 of 2 listeners"), "{e}");
        assert_eq!(get(&c.budget.refused_busy), 3);
        let s = c.budget.status_json();
        assert_eq!(
            (&s["chains"], &s["listeners"], &s["taps"]),
            (&json!(3), &json!(2), &json!(1))
        );
        assert_eq!(s["used_cores"], 0.81);
        drop(t);
        let t2 = Slot::claim(&c, &l, ChainKind::Tap, 10, 0.0).unwrap();
        b.set_mcores(100);
        assert_eq!(get(&c.budget.used_mcores), 510);
        assert_eq!(get(&c.listen.budget_used_mcores), 500, "listen mirror");
        a.release();
        drop((a, b, t2));
        assert_eq!(get(&c.budget.chains), 0);
        assert_eq!(get(&c.budget.used_mcores), 0);
        assert_eq!(get(&c.listen.active), 0);
    }

    #[test]
    fn a_per_kind_limit_above_max_chains_raises_it() {
        let l = BudgetLimits {
            max_chains: 4,
            max_listeners: 64,
            ..limits()
        };
        assert_eq!(l.effective_max_chains(), 64);
    }
}
