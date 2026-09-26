//! A **live** front end: it streams at wall-clock rate and drops whatever the capture thread did
//! not collect in time, exactly as a USB SDR does (T-939).
//!
//! [`radio::Radio`] is read on demand and pausable, which is what makes the control-plane tests
//! deterministic and lossless — and what makes it useless for the question T-939 asks. A HackRF at
//! 20 Msps does not wait: it hands the host 131 072-sample transfers on its own clock, and a host
//! that is late gets a `dropped_before` count instead of the samples. The pipeline's whole
//! backpressure story (the flow gate is off live; the ring never waits for a reader) can only be
//! exercised against a source that behaves that way.
//!
//! So this wraps a scripted radio and adds the two properties of a real front end:
//!
//! - **It is paced.** A block is delivered no earlier than its samples exist at the configured
//!   rate, so a run's stream time tracks wall-clock time and a reader that falls behind falls
//!   behind *in real time* rather than against an infinitely fast replay.
//! - **It drops, and says so.** The inner radio keeps generating on the clock; whatever backs up
//!   beyond [`QUEUE_BLOCKS`] is produced and thrown away — the sample indices advance over it and
//!   the next header carries the count in `dropped_before`. That is the shape of a USB overflow,
//!   and it is the shape `source.source_dropped` counts.
//! - **It is not pausable**, so `Pipeline::start` refuses lossless mode over it and the run takes
//!   the live path under test.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hk_core::{BlockHeader, Source, SourceControl, SourceError};
use num_complex::{Complex, Complex32};

use super::radio::{Generator, Radio, RadioControl};

/// Transfers the front end buffers before it starts dropping (a USB queue depth).
pub const QUEUE_BLOCKS: u64 = 4;

/// A wall-clock-paced live front end. See the module docs.
pub struct Paced {
    inner: Radio,
    rate_hz: f64,
    block: u64,
    started: Option<Instant>,
    /// Samples the front end has generated (delivered **and** dropped).
    produced: u64,
    scratch: Vec<Complex<i8>>,
    stats: Arc<PacedStats>,
}

/// What the front end did, readable from the test thread.
#[derive(Debug, Default)]
pub struct PacedStats {
    /// Samples the front end produced and threw away because the host was late.
    pub dropped: AtomicU64,
    /// Blocks handed to the host.
    pub blocks: AtomicU64,
}

impl PacedStats {
    /// Samples dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Paced {
    /// A front end at `(center_hz, rate_hz)` delivering `block`-sample transfers on its own clock.
    pub fn new(
        center_hz: f64,
        rate_hz: f64,
        block: usize,
        generate: Generator,
    ) -> (Self, Arc<RadioControl>, Arc<PacedStats>) {
        let (inner, control) = Radio::new(center_hz, rate_hz, block, generate);
        let stats = Arc::new(PacedStats::default());
        (
            Self {
                inner,
                rate_hz,
                block: block as u64,
                started: None,
                produced: 0,
                scratch: Vec::with_capacity(block),
                stats: Arc::clone(&stats),
            },
            control,
            stats,
        )
    }

    /// Samples the front end's clock says exist by now.
    fn due(&self, t0: Instant) -> u64 {
        (t0.elapsed().as_secs_f64() * self.rate_hz) as u64
    }
}

impl Source for Paced {
    fn capabilities(&self) -> &hk_core::SourceCapabilities {
        self.inner.capabilities()
    }

    fn control(&self) -> Arc<dyn SourceControl> {
        self.inner.control()
    }

    /// **Never.** A live front end cannot be held, which is the point of this source.
    fn pausable(&self) -> bool {
        false
    }

    fn read_block(
        &mut self,
        samples: &mut Vec<Complex32>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let mut ci8 = Vec::new();
        let h = self.read_block_ci8(&mut ci8)?;
        samples.clear();
        samples.extend(
            ci8.iter()
                .map(|z| Complex32::new(f32::from(z.re) / 128.0, f32::from(z.im) / 128.0)),
        );
        Ok(h)
    }

    fn read_block_ci8(
        &mut self,
        samples: &mut Vec<Complex<i8>>,
    ) -> Result<Option<BlockHeader>, SourceError> {
        let t0 = *self.started.get_or_insert_with(Instant::now);
        // Wait until the front end's own clock has produced a whole transfer.
        while self.due(t0) < self.produced + self.block {
            std::thread::sleep(Duration::from_micros(200));
        }
        // Everything beyond the queue depth is produced and thrown away: the indices move over it
        // and the host is told how much it missed.
        let mut dropped = 0u64;
        while self.due(t0) > self.produced + self.block * QUEUE_BLOCKS {
            match self.inner.read_block_ci8(&mut self.scratch)? {
                Some(_) => {
                    dropped += self.scratch.len() as u64;
                    self.produced += self.scratch.len() as u64;
                }
                None => return Ok(None),
            }
        }
        let Some(mut h) = self.inner.read_block_ci8(samples)? else {
            return Ok(None);
        };
        self.produced += samples.len() as u64;
        h.dropped_before += dropped;
        self.stats.dropped.fetch_add(dropped, Ordering::Relaxed);
        self.stats.blocks.fetch_add(1, Ordering::Relaxed);
        Ok(Some(h))
    }
}
