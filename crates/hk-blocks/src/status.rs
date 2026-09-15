//! Block status readouts (ADR-0011 §1.3).

use serde_json::{Map, Number, Value};

/// Lock state of a block that acquires something (carrier, clock, sync).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lock {
    /// Nothing to lock, or not applicable.
    #[default]
    None,
    /// Acquiring.
    Searching,
    /// Locked.
    Locked,
}

impl Lock {
    /// Wire token.
    pub const fn as_str(self) -> &'static str {
        match self {
            Lock::None => "none",
            Lock::Searching => "searching",
            Lock::Locked => "locked",
        }
    }
}

/// Most block-specific extras in one status.
pub const MAX_EXTRAS: usize = 6;

/// Block-specific numeric readouts (`timing_offset`, `blocks_ok`, …) in a fixed array, so a
/// status stays `Copy` and reading it never allocates. Keys are `[a-z0-9_]+` literals.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Extras {
    items: [(&'static str, f64); MAX_EXTRAS],
    len: u8,
}

impl Extras {
    /// Sets `key` (replacing an existing value). Returns `false` when full.
    pub fn set(&mut self, key: &'static str, value: f64) -> bool {
        let len = self.len as usize;
        if let Some(slot) = self.items[..len].iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
            return true;
        }
        if len == MAX_EXTRAS {
            return false;
        }
        self.items[len] = (key, value);
        self.len += 1;
        true
    }

    /// Entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, f64)> + '_ {
        self.items[..self.len as usize].iter().copied()
    }
}

/// A block's readout. Common fields first so the UI can render any block the same way.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Status {
    /// Lock state.
    pub lock: Lock,
    /// Estimated SNR at this stage, dB.
    pub snr_db: Option<f32>,
    /// Error rate at this stage in `[0, 1]`: BER estimate, block/CRC failure ratio.
    pub error_rate: Option<f32>,
    /// Stage quality in `[0, 1]` (eye opening, sync correlation), for refinement objectives.
    pub quality: Option<f32>,
    /// Items consumed since build (all inputs).
    pub items_in: u64,
    /// Items produced since build (all outputs).
    pub items_out: u64,
    /// Block-specific extras.
    pub extra: Extras,
}

impl Status {
    /// Adds this node's readout to the flat metadata object of a status record
    /// (`docs/stream-contract.md` §14.3) as `<node>.<metric>` keys: numbers, booleans and short
    /// tokens only. The runtime batches every node into **one** record per status tick (about
    /// every 250 ms). Non-finite values are omitted. Allocates: call it at status rate, not per
    /// chunk.
    pub fn to_metadata(&self, node: &str, m: &mut Map<String, Value>) {
        m.insert(format!("{node}.lock"), self.lock.as_str().into());
        let mut num = |k: &str, v: Option<f64>| {
            if let Some(n) = v.and_then(Number::from_f64) {
                m.insert(format!("{node}.{k}"), Value::Number(n));
            }
        };
        num("snr_db", self.snr_db.map(f64::from));
        num("error_rate", self.error_rate.map(f64::from));
        num("quality", self.quality.map(f64::from));
        for (k, v) in self.extra.iter() {
            num(k, Some(v));
        }
        m.insert(format!("{node}.items_in"), self.items_in.into());
        m.insert(format!("{node}.items_out"), self.items_out.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_are_bounded_and_status_flattens() {
        let mut s = Status {
            lock: Lock::Locked,
            error_rate: Some(0.25),
            snr_db: Some(f32::NAN),
            ..Default::default()
        };
        for (i, k) in ["a", "b", "c", "d", "e", "f"].into_iter().enumerate() {
            assert!(s.extra.set(k, i as f64));
        }
        assert!(!s.extra.set("g", 0.0));
        assert!(s.extra.set("a", 9.0));
        let mut m = Map::new();
        s.to_metadata("sync", &mut m);
        Status::default().to_metadata("crc", &mut m);
        assert_eq!(m["sync.lock"], "locked");
        assert_eq!(m["sync.error_rate"], 0.25);
        assert_eq!(m["sync.a"], 9.0);
        assert_eq!(m["crc.lock"], "none");
        assert!(!m.contains_key("sync.snr_db"));
        assert!(hk_stream::policy::metadata_is_allowlist_shaped(
            &Value::Object(m)
        ));
    }
}
