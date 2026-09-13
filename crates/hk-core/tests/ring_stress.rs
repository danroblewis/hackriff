//! Ring stress under a racing writer (regression for the T-003 review: stale reads, meta-ring
//! laps, inexact overrun accounting, startup offsets).
//!
//! A tiny ring (256 samples, 4 metadata records) takes index-encoded samples with random block
//! lengths, random source gaps (some longer than the ring) and random provenance changes, while
//! readers under both resync policies race the writer, some attached before the stream starts
//! (at a counter far beyond the capacity) and some mid-stream. For several seeds:
//!
//! - every delivered sample equals its stream index, and its provenance and block metadata match
//!   the writer's plan;
//! - every stream index from a reader's start to the stream end is accounted exactly once, and
//!   classified correctly against the plan: read, lost (`Overrun::lost_samples`) or source gap
//!   (`dropped_before` / `Overrun::gap_samples`).
//!
//! Bounded to a few seconds; set `HK_STRESS_SEEDS` to run more seeds. Run it in release too
//! (`cargo test --release -p hk-core --test ring_stress`): the original bug showed only there.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use hk_core::{
    BlockHeader, Discontinuity, ProvenanceHandle, ReadOutcome, ResyncPolicy, RingConfig,
    RingHandle, ring_buffer,
};
use hk_model::{SampleTime, Timestamp};
use num_complex::Complex32;

const SAMPLE_CAPACITY: usize = 256;
const BLOCK_CAPACITY: usize = 4;
const BLOCKS_PER_SEED: usize = 30_000;

fn encode(index: u64) -> Complex32 {
    Complex32::new(
        f32::from_bits((index >> 32) as u32),
        f32::from_bits(index as u32),
    )
}

fn decode(s: Complex32) -> u64 {
    (u64::from(s.re.to_bits()) << 32) | u64::from(s.im.to_bits())
}

/// The writer's stream, fixed before any thread starts.
struct Plan {
    /// `(first sample, length, provenance index)` per block.
    blocks: Vec<(u64, u64, usize)>,
    /// Samples produced before block `i` (one extra entry for the total).
    produced_before: Vec<u64>,
    provenance: Vec<ProvenanceHandle>,
    /// Blocks after which the writer pauses so readers catch up.
    pauses: Vec<bool>,
}

impl Plan {
    fn new(seed: u64) -> Self {
        let mut rng = common::Rng::new(seed);
        let provenance: Vec<_> = (0..6)
            .map(|k| {
                ProvenanceHandle::new(common::provenance(
                    "synthetic:ring-stress",
                    100e6 + f64::from(k) * 1e6,
                    2e6,
                ))
            })
            .collect();
        // The stream starts far beyond the ring capacity (a replay with core:global_index).
        let mut first = 5_000_000 + rng.next_u64() % 1000;
        let mut prov = 0;
        let mut blocks = Vec::with_capacity(BLOCKS_PER_SEED);
        let mut produced_before = Vec::with_capacity(BLOCKS_PER_SEED + 1);
        let mut pauses = Vec::with_capacity(BLOCKS_PER_SEED);
        let mut produced = 0;
        for _ in 0..BLOCKS_PER_SEED {
            let r = rng.next_u64();
            if !blocks.is_empty() && r % 8 == 0 {
                first += 1 + (r >> 8) % 400;
            }
            if (r >> 20) % 5 == 0 {
                prov = ((r >> 24) % 6) as usize;
            }
            let len = 1 + (r >> 32) % 64;
            blocks.push((first, len, prov));
            produced_before.push(produced);
            pauses.push((r >> 40) % 2000 == 0);
            produced += len;
            first += len;
        }
        produced_before.push(produced);
        Self {
            blocks,
            produced_before,
            provenance,
            pauses,
        }
    }

    fn start(&self) -> u64 {
        self.blocks[0].0
    }

    fn end(&self) -> u64 {
        let (first, len, _) = self.blocks[self.blocks.len() - 1];
        first + len
    }

    /// Samples produced with stream index below `x`.
    fn produced_below(&self, x: u64) -> u64 {
        let i = self.blocks.partition_point(|&(f, l, _)| f + l <= x);
        let mut n = self.produced_before[i];
        if let Some(&(f, l, _)) = self.blocks.get(i) {
            if x > f {
                n += (x - f).min(l);
            }
        }
        n
    }

    /// Source-gap indices (never produced) in `[a, b)`.
    fn gaps_in(&self, a: u64, b: u64) -> u64 {
        (b - a) - (self.produced_below(b) - self.produced_below(a))
    }

    /// The block holding stream index `i`.
    fn block_of(&self, i: u64) -> Option<usize> {
        let k = self.blocks.partition_point(|&(f, l, _)| f + l <= i);
        self.blocks.get(k).filter(|&&(f, _, _)| f <= i).map(|_| k)
    }
}

#[derive(Default)]
struct Tally {
    read: u64,
    lost: u64,
    gaps: u64,
    overruns: u64,
}

enum Attach {
    /// `RingHandle::reader` before the first push.
    Live,
    /// `RingHandle::reader_at(sample)` once the stream is running.
    At(u64),
}

fn run_reader(
    ring: RingHandle<Complex32>,
    plan: Arc<Plan>,
    policy: ResyncPolicy,
    buf_len: usize,
    attach: Attach,
    label: String,
) -> Tally {
    let (reader, start, exact_first) = match attach {
        Attach::Live => (ring.reader(), plan.start(), true),
        // Where history is already gone at attach, block boundaries are unknown: that first
        // overrun may count gap indices as lost.
        Attach::At(s) => (ring.reader_at(s), s.max(plan.start()), false),
    };
    let mut reader = reader.with_resync_policy(policy);
    let mut buf = vec![Complex32::default(); buf_len];
    let mut pos = start;
    let mut tally = Tally::default();
    let mut delivered_any = false;
    loop {
        match reader.read_timeout(&mut buf, Duration::from_millis(50)) {
            ReadOutcome::Data(c) => {
                let first = c.first_sample();
                assert_eq!(
                    first,
                    pos + c.dropped_before,
                    "{label}: chunk start vs position"
                );
                assert_eq!(
                    plan.gaps_in(pos, first),
                    c.dropped_before,
                    "{label}: dropped_before must be exactly the source gap before {first}"
                );
                let k = plan
                    .block_of(first)
                    .unwrap_or_else(|| panic!("{label}: {first} is not a produced sample"));
                let (bf, bl, bp) = plan.blocks[k];
                assert!(
                    c.end_sample() <= bf + bl,
                    "{label}: chunk spans a block end"
                );
                assert_eq!(
                    c.block_start,
                    first == bf,
                    "{label}: block_start at {first}"
                );
                if !c.block_start {
                    assert_eq!(c.dropped_before, 0, "{label}");
                } else {
                    let leading_gap = k > 0 && {
                        let (pf, pl, _) = plan.blocks[k - 1];
                        bf > pf + pl
                    };
                    assert_eq!(
                        c.discontinuity.contains(Discontinuity::GAP),
                        leading_gap,
                        "{label}: GAP flag of block {k}"
                    );
                }
                assert_eq!(
                    c.provenance, plan.provenance[bp],
                    "{label}: provenance of {first}"
                );
                for (j, s) in buf[..c.len].iter().enumerate() {
                    let got = decode(*s);
                    assert_eq!(
                        got,
                        first + j as u64,
                        "{label}: stale sample at {} (chunk {first}+{})",
                        first + j as u64,
                        c.len
                    );
                }
                tally.read += c.len as u64;
                tally.gaps += c.dropped_before;
                pos = c.end_sample();
                delivered_any = true;
            }
            ReadOutcome::Overrun {
                lost_samples,
                gap_samples,
                resume_at,
            } => {
                assert_eq!(
                    resume_at,
                    pos + lost_samples + gap_samples,
                    "{label}: overrun span"
                );
                let true_gaps = plan.gaps_in(pos, resume_at);
                if exact_first || delivered_any || tally.overruns > 0 {
                    assert_eq!(
                        gap_samples, true_gaps,
                        "{label}: overrun gap/lost split over [{pos}, {resume_at})"
                    );
                } else {
                    assert!(gap_samples <= true_gaps, "{label}");
                }
                tally.lost += lost_samples;
                tally.gaps += gap_samples;
                tally.overruns += 1;
                pos = resume_at;
            }
            ReadOutcome::Empty => {}
            ReadOutcome::Closed => break,
        }
    }
    assert_eq!(pos, plan.end(), "{label}: drained to the stream end");
    assert_eq!(
        tally.read + tally.lost + tally.gaps,
        plan.end() - start,
        "{label}: every index accounted exactly once"
    );
    assert_eq!(
        (
            reader.samples_read(),
            reader.lost_samples(),
            reader.gap_samples(),
            reader.overruns()
        ),
        (tally.read, tally.lost, tally.gaps, tally.overruns),
        "{label}: reader counters agree with the reported events"
    );
    tally
}

fn run_seed(seed: u64) -> Tally {
    let plan = Arc::new(Plan::new(seed));
    let (mut writer, ring) = ring_buffer::<Complex32>(RingConfig {
        sample_capacity: SAMPLE_CAPACITY,
        block_capacity: BLOCK_CAPACITY,
    });
    let mut readers = Vec::new();
    for (i, (policy, buf_len)) in [
        (ResyncPolicy::Latest, 1),
        (ResyncPolicy::Latest, 64),
        (ResyncPolicy::Oldest, 7),
        (ResyncPolicy::Oldest, 300),
    ]
    .into_iter()
    .enumerate()
    {
        let (ring, plan) = (ring.clone(), Arc::clone(&plan));
        let label = format!("seed {seed} live reader {i} ({policy:?}, buf {buf_len})");
        readers.push(thread::spawn(move || {
            run_reader(ring, plan, policy, buf_len, Attach::Live, label)
        }));
    }

    let writer_plan = Arc::clone(&plan);
    let writer_thread = thread::spawn(move || {
        let mut samples = vec![Complex32::default(); 64];
        for (k, &(first, len, prov)) in writer_plan.blocks.iter().enumerate() {
            for (j, s) in samples[..len as usize].iter_mut().enumerate() {
                *s = encode(first + j as u64);
            }
            let header = BlockHeader {
                time: SampleTime {
                    sample_index: first,
                    host_time: Timestamp::from_unix_nanos(first as i64 * 500),
                },
                provenance: writer_plan.provenance[prov].clone(),
                discontinuity: Discontinuity::NONE,
                dropped_before: 0,
            };
            writer.push(&header, &samples[..len as usize]).unwrap();
            if writer_plan.pauses[k] {
                thread::sleep(Duration::from_millis(1));
            } else if k % 32 == 0 {
                thread::yield_now();
            }
        }
    });

    // Mid-stream attaches: at recent history, older history and the future.
    for (i, back) in [(0, 200u64), (1, 5000), (2, 0)] {
        thread::sleep(Duration::from_millis(2));
        let from = match ring.next_sample() {
            Some(next) if back == 0 => (next + 100).min(plan.end()),
            Some(next) => next.saturating_sub(back),
            None => 0,
        };
        let (ring, plan) = (ring.clone(), Arc::clone(&plan));
        let policy = if i == 1 {
            ResyncPolicy::Latest
        } else {
            ResyncPolicy::Oldest
        };
        let label = format!("seed {seed} reader_at({from}) {i} ({policy:?})");
        readers.push(thread::spawn(move || {
            run_reader(ring, plan, policy, 50, Attach::At(from), label)
        }));
    }

    writer_thread.join().unwrap();
    let mut total = Tally::default();
    for r in readers {
        let t = r.join().unwrap();
        total.read += t.read;
        total.lost += t.lost;
        total.gaps += t.gaps;
        total.overruns += t.overruns;
    }
    total
}

#[test]
fn racing_readers_get_exact_samples_and_exact_loss_accounting() {
    let seeds: u64 = std::env::var("HK_STRESS_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let mut total = Tally::default();
    for seed in 0..seeds {
        let t = run_seed(0x5eed_0000 + seed);
        total.read += t.read;
        total.lost += t.lost;
        total.gaps += t.gaps;
        total.overruns += t.overruns;
    }
    // The run must have exercised both delivery and laps.
    assert!(total.read > 0, "no samples delivered");
    assert!(total.overruns > 0, "no reader was lapped");
    assert!(total.gaps > 0, "no source gaps crossed");
    eprintln!(
        "ring stress: {seeds} seeds, {} read, {} lost, {} gap indices, {} overruns",
        total.read, total.lost, total.gaps, total.overruns
    );
}
