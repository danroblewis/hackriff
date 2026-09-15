//! `consensus`: commits field values only once enough agreeing frames back them, and withholds
//! every other value, so a false FEC correction (or a rare undetected error) never surfaces as a
//! station identity or text (T-210).
//!
//! **Slots.** One slot per configured field (and per `address` value for segmented fields such
//! as RDS PS characters by segment), plus one for the `key` (identity, e.g. PI). A slot holds the
//! last `window` observations of that field as (raw bits, weight): a frame whose check is `valid`
//! weighs `clean_weight`, any other non-invalid frame (`corrected`, `no-crc`, `unknown`) weighs
//! `corrected_weight`; `invalid` frames and frames without a layer tree pass through untouched and
//! are not counted.
//!
//! **Commit rule.** When an observation's value reaches `commit_weight` summed over the slot's
//! window, it becomes the slot's committed value (it stays committed until another value does).
//! A field node whose value is not its slot's committed value is **withheld** in the output: the
//! frame's layer tree is copied with that node's `value` and `text` cleared and `error` set, so
//! every consumer (`text`, `messages` outputs) treats it as absent. Bytes, check status and the
//! other nodes pass through unchanged. With the RDS defaults (clean 2, corrected 1, commit 3,
//! window 8) a value commits after two agreeing observations of which at least one is clean, or
//! three corrected ones; a single frame never commits anything.
//!
//! **Key gate.** With a `key`, every other field is counted and passed only while the frame's
//! key equals the committed key (a frame with a withheld or missing key withholds all its
//! fields); a newly committed, different key clears the other slots (a retune to another station
//! restarts consensus without mixing its fields with the previous station's). Chunk flags do not
//! clear state: the key gate already separates sources, and a looping or briefly interrupted
//! stream keeps its evidence.
//!
//! Status: extras `frames`, `frames_withheld` (frames with at least one withheld field),
//! `fields_committed`, `fields_withheld`, `slots`; `quality` = committed share of counted field
//! observations.

use std::sync::Arc;

use hk_model::CrcStatus;
use hk_recipe::{BlockDescriptor, Params, PortType};
use hk_stream::inspector::LayerTree;
use serde_json::Value;

use crate::block::{Block, BlockError, Io, ParamUpdate, PortInfo};
use crate::blocks::framing::common::read_bits;
use crate::buffer::{ChunkMeta, Frame, FrameBuf, PortSlice, PortVec};
use crate::registry::{BlockFactory, BuildCtx};
use crate::status::Status;

/// Most slots kept; observations for new slots beyond it are withheld.
const MAX_SLOTS: usize = 1024;
/// Largest `window`.
const MAX_WINDOW: usize = 32;

/// Builds [`Consensus`].
pub struct ConsensusFactory {
    descriptor: BlockDescriptor,
}

impl ConsensusFactory {
    /// The factory (descriptor pinned in [`super::planned`]).
    pub fn new() -> Self {
        Self {
            descriptor: super::pinned("consensus"),
        }
    }
}

impl Default for ConsensusFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl BlockFactory for ConsensusFactory {
    fn descriptor(&self) -> &BlockDescriptor {
        &self.descriptor
    }

    fn build(&self, params: &Params, _ctx: &BuildCtx<'_>) -> Result<Box<dyn Block>, BlockError> {
        Ok(Box::new(Consensus::new(
            Config::parse(params)?,
            params.clone(),
        )))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Config {
    key: Option<String>,
    /// (field path, address path).
    fields: Vec<(String, Option<String>)>,
    clean: u16,
    corrected: u16,
    commit: u32,
    window: usize,
}

fn bad(msg: &str) -> BlockError {
    BlockError::Params(msg.to_owned())
}

impl Config {
    fn parse(p: &Params) -> Result<Self, BlockError> {
        let key = match p.get("key") {
            None => None,
            Some(v) => Some(
                v.as_str()
                    .ok_or_else(|| bad("key is a field path"))?
                    .to_owned(),
            ),
        };
        let mut fields = Vec::new();
        for f in p
            .get("fields")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let field = f
                .get("field")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("fields[].field is a field path"))?;
            let address = match f.get("address") {
                None => None,
                Some(a) => Some(
                    a.as_str()
                        .ok_or_else(|| bad("fields[].address is a field path"))?
                        .to_owned(),
                ),
            };
            fields.push((field.to_owned(), address));
        }
        if key.is_none() && fields.is_empty() {
            return Err(bad("consensus needs a key or fields"));
        }
        let int = |k: &str, d: u64, lo: u64, hi: u64| -> Result<u64, BlockError> {
            match p.get(k) {
                None => Ok(d),
                Some(v) => v
                    .as_u64()
                    .filter(|x| (lo..=hi).contains(x))
                    .ok_or_else(|| bad(&format!("{k} is {lo}..={hi}"))),
            }
        };
        let clean = int("clean_weight", 2, 1, 16)? as u16;
        let corrected = int("corrected_weight", 1, 0, 16)? as u16;
        let commit = int("commit_weight", 3, 1, 64)? as u32;
        let window = int("window", 8, 1, MAX_WINDOW as u64)? as usize;
        if u32::from(corrected) >= commit {
            return Err(bad(
                "corrected_weight must be below commit_weight (one corrected frame would commit)",
            ));
        }
        Ok(Self {
            key,
            fields,
            clean,
            corrected,
            commit,
            window,
        })
    }
}

#[derive(Clone, Debug)]
struct Slot {
    /// 0: the key; `i + 1`: `fields[i]`.
    field: usize,
    address: u64,
    obs: [(u64, u16); MAX_WINDOW],
    len: usize,
    pos: usize,
    committed: Option<u64>,
}

impl Slot {
    fn new(field: usize, address: u64) -> Self {
        Self {
            field,
            address,
            obs: [(0, 0); MAX_WINDOW],
            len: 0,
            pos: 0,
            committed: None,
        }
    }

    /// Adds one observation; whether `value` is committed afterwards.
    fn observe(&mut self, value: u64, weight: u16, window: usize, commit: u32) -> bool {
        self.obs[self.pos] = (value, weight);
        self.pos = (self.pos + 1) % window;
        self.len = (self.len + 1).min(window);
        let agree: u32 = self.obs[..self.len]
            .iter()
            .filter(|o| o.0 == value)
            .map(|o| u32::from(o.1))
            .sum();
        if agree >= commit {
            self.committed = Some(value);
        }
        self.committed == Some(value)
    }
}

/// The block.
pub struct Consensus {
    cfg: Config,
    params: Params,
    slots: Vec<Slot>,
    withheld: Vec<u32>,
    frames: u64,
    frames_withheld: u64,
    fields_committed: u64,
    fields_withheld: u64,
    status: Status,
}

/// A present field's node id and raw bits (fields wider than 64 bits are not comparable).
fn read(tree: &LayerTree, bytes: &[u8], bit_len: u32, path: &str) -> Option<(u32, u64)> {
    let n = tree.node(path).filter(|n| !n.error && n.value.is_some())?;
    let [off, len] = n.bits;
    if len > 64 || off + len > bit_len {
        return None;
    }
    Some((n.id, read_bits(bytes, off as usize, len as usize)))
}

impl Consensus {
    fn new(cfg: Config, params: Params) -> Self {
        Self {
            cfg,
            params,
            slots: Vec::new(),
            withheld: Vec::new(),
            frames: 0,
            frames_withheld: 0,
            fields_committed: 0,
            fields_withheld: 0,
            status: Status::default(),
        }
    }

    /// Observes `value` in slot (`field`, `address`); whether it is committed.
    fn observe(&mut self, field: usize, address: u64, value: u64, weight: u16) -> bool {
        let (window, commit) = (self.cfg.window, self.cfg.commit);
        let i = match self
            .slots
            .iter()
            .position(|s| s.field == field && s.address == address)
        {
            Some(i) => i,
            None if self.slots.len() < MAX_SLOTS => {
                self.slots.push(Slot::new(field, address));
                self.slots.len() - 1
            }
            None => return false,
        };
        self.slots[i].observe(value, weight, window, commit)
    }

    fn take(&mut self, f: Frame<'_>, out: &mut FrameBuf) {
        let Some(tree) = f
            .info
            .layers
            .as_ref()
            .filter(|_| f.info.check != CrcStatus::Invalid)
        else {
            out.push(f.bytes, f.info.clone());
            return;
        };
        self.frames += 1;
        let weight = if f.info.check == CrcStatus::Valid {
            self.cfg.clean
        } else {
            self.cfg.corrected
        };
        let bit_len = f.info.bit_len;
        self.withheld.clear();
        let key_ok = match self.cfg.key.clone() {
            None => true,
            Some(k) => match read(tree, f.bytes, bit_len, &k) {
                None => false,
                Some((id, v)) => {
                    let before = self
                        .slots
                        .iter()
                        .find(|s| s.field == 0)
                        .and_then(|s| s.committed);
                    let agrees = self.observe(0, 0, v, weight);
                    if agrees && before.is_some_and(|b| b != v) {
                        self.slots.retain(|s| s.field == 0);
                    }
                    if agrees {
                        self.fields_committed += 1;
                    } else {
                        self.withheld.push(id);
                    }
                    agrees
                }
            },
        };
        for i in 0..self.cfg.fields.len() {
            let (field, address) = &self.cfg.fields[i];
            let Some((id, v)) = read(tree, f.bytes, bit_len, field) else {
                continue;
            };
            let addr = match address {
                None => Some(0),
                Some(a) => read(tree, f.bytes, bit_len, a).map(|(_, x)| x),
            };
            let committed = match addr {
                Some(addr) if key_ok => self.observe(i + 1, addr, v, weight),
                _ => false,
            };
            if committed {
                self.fields_committed += 1;
            } else {
                self.withheld.push(id);
            }
        }
        if self.withheld.is_empty() {
            out.push(f.bytes, f.info.clone());
            return;
        }
        self.frames_withheld += 1;
        self.fields_withheld += self.withheld.len() as u64;
        let mut t = (**tree).clone();
        for &id in &self.withheld {
            if let Some(n) = t.nodes.get_mut(id as usize) {
                n.value = None;
                n.text = None;
                n.error = true;
            }
        }
        let mut info = f.info.clone();
        info.layers = Some(Arc::new(t));
        out.push(f.bytes, info);
    }
}

impl Block for Consensus {
    fn init(&mut self, inputs: &[PortInfo]) -> Result<Vec<PortInfo>, BlockError> {
        let [input] = inputs else {
            return Err(BlockError::Ports("consensus has exactly one input".into()));
        };
        if input.ty != PortType::Frames {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.ty,
            });
        }
        Ok(vec![*input])
    }

    fn process(&mut self, io: &mut Io<'_>) -> Result<(), BlockError> {
        let input = io.input(0)?;
        let PortSlice::Frames(frames) = input.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: input.data.port_type(),
            });
        };
        let out = io.output(0)?;
        out.meta = ChunkMeta {
            index: out.meta.index,
            ..input.meta
        };
        let PortVec::Frames(buf) = &mut out.data else {
            return Err(BlockError::PortType {
                port: 0,
                expected: PortType::Frames,
                got: out.data.port_type(),
            });
        };
        for f in frames.iter() {
            self.take(f, buf);
        }
        let n = frames.len() as u64;
        let s = &mut self.status;
        s.items_in += n;
        s.items_out += n;
        let counted = self.fields_committed + self.fields_withheld;
        s.quality = (counted > 0).then(|| self.fields_committed as f32 / counted as f32);
        s.extra.set("frames", self.frames as f64);
        s.extra.set("frames_withheld", self.frames_withheld as f64);
        s.extra
            .set("fields_committed", self.fields_committed as f64);
        s.extra.set("fields_withheld", self.fields_withheld as f64);
        s.extra.set("slots", self.slots.len() as f64);
        Ok(())
    }

    fn reset(&mut self) {
        self.slots.clear();
    }

    fn update_params(
        &mut self,
        params: &Params,
        _ctx: &BuildCtx<'_>,
    ) -> Result<ParamUpdate, BlockError> {
        Config::parse(params)?;
        Ok(if *params == self.params {
            ParamUpdate::Applied
        } else {
            ParamUpdate::Rebuild
        })
    }

    fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_commits_two_agreeing_with_one_clean_or_three_corrected_never_one() {
        let (w, c) = (8, 3);
        // One clean: no. Clean + corrected: yes.
        let mut s = Slot::new(1, 0);
        assert!(!s.observe(7, 2, w, c));
        assert!(s.observe(7, 1, w, c));
        // Two corrected: no; the third: yes.
        let mut s = Slot::new(1, 0);
        assert!(!s.observe(9, 1, w, c));
        assert!(!s.observe(9, 1, w, c));
        assert!(s.observe(9, 1, w, c));
        // A committed value stays until another reaches the threshold; stray values are withheld.
        assert!(!s.observe(4, 1, w, c));
        assert!(s.observe(9, 1, w, c));
        assert!(!s.observe(5, 2, w, c));
        assert!(s.observe(5, 2, w, c));
        // 9 still has three agreeing observations inside the window, so it commits again: the
        // committed value is the last one to reach the threshold.
        assert!(s.observe(9, 1, w, c));
        // Agreement older than the window does not count: corrected 6s spread 8 apart never
        // commit.
        let mut s = Slot::new(1, 0);
        for _ in 0..5 {
            assert!(!s.observe(6, 1, w, c));
            for k in 0..7 {
                s.observe(100 + k, 1, w, c);
            }
        }
    }
}
