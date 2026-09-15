//! Radio time per share over a sliding window of device/sample-clock time (ADR-0012 §0, §5.3):
//! the sweep floor ("≥ 25 % of radio time over any 10-min window") and the exploration floor
//! ("15 % of bandit time") are checked against it. Fixed ring of time buckets, allocated once.

/// What a step's radio time counts towards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Share {
    /// Discovery: sweep hops and dwell-only region windows.
    Discovery = 0,
    /// Bandit exploitation, POI dwells and verification groups.
    Exploit = 1,
    /// Bandit exploration (stalest arm, starvation bound).
    Explore = 2,
    /// Every higher tier (scheduled plans, leases, interactive).
    Other = 3,
}

/// Nanoseconds per [`Share`] over the window.
pub(crate) type ShareNs = [i64; 4];

/// A ring of `bucket_ns` buckets covering the trailing window plus planned time ahead of now.
#[derive(Clone, Debug)]
pub(crate) struct TierWindow {
    bucket_ns: i64,
    window_buckets: i64,
    ahead_buckets: i64,
    ids: Vec<i64>,
    ns: Vec<ShareNs>,
}

impl TierWindow {
    /// `window_ns` trailing, `ahead_ns` of planned time ahead of now, `bucket_ns` resolution.
    pub(crate) fn new(window_ns: i64, ahead_ns: i64, bucket_ns: i64) -> Self {
        let bucket_ns = bucket_ns.max(1);
        let window_buckets = (window_ns / bucket_ns).max(1);
        let ahead_buckets = (ahead_ns / bucket_ns).max(1) + 1;
        let n = (window_buckets + ahead_buckets + 1) as usize;
        Self {
            bucket_ns,
            window_buckets,
            ahead_buckets,
            ids: vec![i64::MIN; n],
            ns: vec![[0; 4]; n],
        }
    }

    fn slot(&mut self, id: i64) -> &mut ShareNs {
        let i = id.rem_euclid(self.ids.len() as i64) as usize;
        if self.ids[i] != id {
            self.ids[i] = id;
            self.ns[i] = [0; 4];
        }
        &mut self.ns[i]
    }

    /// Adds (`sign` 1) or removes (`sign` −1) `[start, start + dur)` to `share`, split over the
    /// buckets it spans. Time beyond the ring's reach ahead of `start` is not counted.
    pub(crate) fn add(&mut self, start_ns: i64, dur_ns: i64, share: Share, sign: i64) {
        if dur_ns <= 0 {
            return;
        }
        let first = start_ns.div_euclid(self.bucket_ns);
        let last_allowed = first + self.ahead_buckets;
        let end_ns = start_ns.saturating_add(dur_ns);
        let mut t = start_ns;
        let mut id = first;
        while t < end_ns && id <= last_allowed {
            let bucket_end = (id + 1).saturating_mul(self.bucket_ns);
            let part = end_ns.min(bucket_end) - t;
            self.slot(id)[share as usize] += sign * part;
            t += part;
            id += 1;
        }
    }

    /// Totals over the trailing window ending at `now_ns` plus planned time ahead of it.
    pub(crate) fn totals(&self, now_ns: i64) -> ShareNs {
        let now_b = now_ns.div_euclid(self.bucket_ns);
        let (lo, hi) = (now_b - self.window_buckets + 1, now_b + self.ahead_buckets);
        let mut out = [0i64; 4];
        for (id, ns) in self.ids.iter().zip(&self.ns) {
            if (lo..=hi).contains(id) {
                for (o, v) in out.iter_mut().zip(ns) {
                    *o += (*v).max(0);
                }
            }
        }
        out
    }

    /// Window length, ns.
    pub(crate) fn window_ns(&self) -> i64 {
        self.window_buckets * self.bucket_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn splits_over_buckets_expires_and_removes() {
        let mut w = TierWindow::new(600 * S, 120 * S, 10 * S);
        w.add(5 * S, 20 * S, Share::Exploit, 1);
        w.add(25 * S, 5 * S, Share::Discovery, 1);
        assert_eq!(w.totals(30 * S), [5 * S, 20 * S, 0, 0]);
        w.add(28 * S, 2 * S, Share::Discovery, -1);
        assert_eq!(w.totals(30 * S)[0], 3 * S);
        // 10 minutes later the early buckets have left the window.
        // At 615 s only the 20–30 s bucket remains (5 s of exploit, 3 s of discovery).
        assert_eq!(w.totals(615 * S), [3 * S, 5 * S, 0, 0]);
        assert_eq!(w.totals(700 * S), [0; 4]);
        assert_eq!(w.window_ns(), 600 * S);
    }
}
