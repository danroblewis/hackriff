//! Record framing (docs/stream-contract.md §3): every frame is a `u32` little-endian payload length
//! followed by that many payload bytes. The length never exceeds the stream's `max_frame_len`,
//! which never exceeds [`MAX_FRAME_LEN`].
//!
//! [`FrameDecoder`] is incremental: bytes may arrive split at any point. A length prefix larger
//! than the limit is an error as soon as the four prefix bytes are seen, before any buffer is
//! sized for it, and the decoder stays failed (a byte stream cannot resynchronise).

use std::io::{self, Read};

/// Bytes in the length prefix.
pub const LEN_PREFIX: usize = 4;

/// Protocol maximum payload length of one frame (4 MiB). A header may declare a smaller limit.
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;

/// Largest header frame a reader accepts before it knows the stream's own limit (64 KiB).
pub const HEADER_MAX_LEN: u32 = 64 * 1024;

/// How much [`FrameDecoder::read_from`] reads per call.
const READ_CHUNK: usize = 64 * 1024;

/// Framing errors.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// A frame (declared or offered) is longer than the limit.
    #[error("frame length {len} exceeds the limit {max}")]
    Oversize {
        /// Declared or offered payload length.
        len: u64,
        /// The limit in force.
        max: u32,
    },
}

/// The length prefix for a payload of `len` bytes, refusing payloads over `max`.
pub fn frame_prefix(len: usize, max: u32) -> Result<[u8; LEN_PREFIX], FrameError> {
    let max = max.min(MAX_FRAME_LEN);
    if len > max as usize {
        return Err(FrameError::Oversize {
            len: len as u64,
            max,
        });
    }
    Ok((len as u32).to_le_bytes())
}

/// Appends one frame (prefix + payload) to `out`.
pub fn encode_frame(out: &mut Vec<u8>, payload: &[u8], max: u32) -> Result<(), FrameError> {
    out.extend_from_slice(&frame_prefix(payload.len(), max)?);
    out.extend_from_slice(payload);
    Ok(())
}

/// Incremental frame decoder, robust to arbitrary split points.
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max: u32,
    failed: Option<FrameError>,
}

impl FrameDecoder {
    /// A decoder accepting frames up to `max_frame_len` (clamped to [`MAX_FRAME_LEN`]).
    pub fn new(max_frame_len: u32) -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            max: max_frame_len.min(MAX_FRAME_LEN),
            failed: None,
        }
    }

    /// Changes the limit, e.g. from [`HEADER_MAX_LEN`] to the header's `max_frame_len`.
    pub fn set_max_frame_len(&mut self, max_frame_len: u32) {
        self.max = max_frame_len.min(MAX_FRAME_LEN);
    }

    /// The limit in force.
    pub fn max_frame_len(&self) -> u32 {
        self.max
    }

    /// Bytes received but not yet returned as frames.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    fn compact(&mut self) {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    /// Adds received bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.compact();
        self.buf.extend_from_slice(data);
    }

    /// Reads once from `r` (at most 64 KiB) into the buffer. `Ok(0)` is end of stream.
    pub fn read_from<R: Read + ?Sized>(&mut self, r: &mut R) -> io::Result<usize> {
        self.compact();
        let old = self.buf.len();
        self.buf.resize(old + READ_CHUNK, 0);
        let result = loop {
            match r.read(&mut self.buf[old..]) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other,
            }
        };
        let n = *result.as_ref().unwrap_or(&0);
        self.buf.truncate(old + n);
        result
    }

    /// The next complete frame's payload, `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<&[u8]>, FrameError> {
        if let Some(e) = &self.failed {
            return Err(e.clone());
        }
        let avail = self.buf.len() - self.start;
        if avail < LEN_PREFIX {
            return Ok(None);
        }
        let prefix: [u8; LEN_PREFIX] = self.buf[self.start..self.start + LEN_PREFIX]
            .try_into()
            .expect("four bytes");
        let len = u32::from_le_bytes(prefix);
        if len > self.max {
            let e = FrameError::Oversize {
                len: len as u64,
                max: self.max,
            };
            self.failed = Some(e.clone());
            return Err(e);
        }
        let total = LEN_PREFIX + len as usize;
        if avail < total {
            return Ok(None);
        }
        let payload = self.start + LEN_PREFIX;
        self.start += total;
        Ok(Some(&self.buf[payload..payload + len as usize]))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// SplitMix64: a tiny deterministic generator so tests need no `rand` dependency.
    pub(crate) struct SplitMix(pub u64);

    impl SplitMix {
        pub(crate) fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        pub(crate) fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    #[test]
    fn round_trip_with_randomized_split_points() {
        let mut rng = SplitMix(0x5EED);
        for trial in 0..200 {
            let count = 1 + rng.below(40);
            let frames: Vec<Vec<u8>> = (0..count)
                .map(|_| {
                    // Include empty frames and frames longer than the read chunk.
                    let len = match rng.below(10) {
                        0 => 0,
                        1 => 70_000 + rng.below(10_000),
                        _ => rng.below(300),
                    };
                    (0..len).map(|_| rng.next_u64() as u8).collect()
                })
                .collect();
            let mut wire = Vec::new();
            for f in &frames {
                encode_frame(&mut wire, f, MAX_FRAME_LEN).unwrap();
            }
            let mut dec = FrameDecoder::new(MAX_FRAME_LEN);
            let mut got = Vec::new();
            let mut pos = 0;
            while pos < wire.len() {
                // Split sizes from 1 byte upwards, sometimes whole frames at once.
                let step = if rng.below(4) == 0 {
                    1 + rng.below(3)
                } else {
                    1 + rng.below(100_000)
                };
                let end = (pos + step).min(wire.len());
                dec.push(&wire[pos..end]);
                pos = end;
                while let Some(f) = dec.next_frame().unwrap() {
                    got.push(f.to_vec());
                }
            }
            assert_eq!(got, frames, "trial {trial}");
            assert_eq!(dec.buffered(), 0);
        }
    }

    #[test]
    fn read_from_handles_short_reads() {
        struct Trickle<'a>(&'a [u8]);
        impl Read for Trickle<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.0.len().min(buf.len()).min(3);
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let mut wire = Vec::new();
        for i in 0..50u8 {
            encode_frame(&mut wire, &[i; 17], 1024).unwrap();
        }
        let mut src = Trickle(&wire);
        let mut dec = FrameDecoder::new(1024);
        let mut n = 0u8;
        while dec.read_from(&mut src).unwrap() > 0 {
            while let Some(f) = dec.next_frame().unwrap() {
                assert_eq!(f, &[n; 17]);
                n += 1;
            }
        }
        assert_eq!(n, 50);
    }

    #[test]
    fn oversize_frame_is_rejected_without_allocating() {
        let mut dec = FrameDecoder::new(1024);
        // Declares 4 GiB - 1; only the prefix arrives.
        dec.push(&u32::MAX.to_le_bytes());
        assert_eq!(
            dec.next_frame(),
            Err(FrameError::Oversize {
                len: u32::MAX as u64,
                max: 1024
            })
        );
        assert!(dec.buf.capacity() < 1024, "no buffer sized for the frame");
        // The decoder stays failed.
        dec.push(&[0; 8]);
        assert!(dec.next_frame().is_err());

        // Exactly the limit is fine; one over is not.
        let mut ok = FrameDecoder::new(8);
        ok.push(&8u32.to_le_bytes());
        ok.push(&[1; 8]);
        assert_eq!(ok.next_frame().unwrap(), Some(&[1u8; 8][..]));
        let mut over = FrameDecoder::new(8);
        over.push(&9u32.to_le_bytes());
        assert!(over.next_frame().is_err());

        // The encoder refuses too, and clamps any limit to the protocol maximum.
        let mut out = Vec::new();
        assert!(encode_frame(&mut out, &[0; 9], 8).is_err());
        assert!(out.is_empty());
        assert!(frame_prefix(MAX_FRAME_LEN as usize + 1, u32::MAX).is_err());
    }
}
