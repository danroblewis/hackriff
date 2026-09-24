//! What a sink block hands the runtime (ADR-0011 §8.4). A sink has an input and no output port,
//! so its product leaves the graph through [`crate::Block::audio_frames`] rather than a port
//! buffer: the runtime reads it after `process` and clears it once published.

/// One finished audio frame's framing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioFrame {
    /// Index of the frame's first sample in the stream's own output-rate sample domain, from 0
    /// at the stream's first sample. A gap (closed squelch, a live-edge skip, lost samples)
    /// shows as a jump — the stream contract §12.2 `sample_index` rule.
    pub sample_index: u64,
    /// Source (ring) sample index of the frame's first sample, for its wall-clock time.
    pub source_index: f64,
    /// The frame follows a gap: the record carries `DISCONTINUITY`.
    pub discontinuity: bool,
    /// Byte offset of the frame's payload in the arena.
    start: usize,
}

/// Finished frames of encoded audio in one arena, pre-sized at `init` so filling it never
/// allocates on the pipeline thread.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioFrames {
    frame_bytes: usize,
    payload: Vec<u8>,
    frames: Vec<AudioFrame>,
}

impl AudioFrames {
    /// Room for `max_frames` frames of `frame_bytes` bytes.
    pub fn with_capacity(frame_bytes: usize, max_frames: usize) -> Self {
        Self {
            frame_bytes,
            payload: Vec::with_capacity(frame_bytes * max_frames),
            frames: Vec::with_capacity(max_frames),
        }
    }

    /// Appends a frame whose payload `encode` writes (exactly `frame_bytes` bytes are kept).
    pub fn push_with(
        &mut self,
        sample_index: u64,
        source_index: f64,
        discontinuity: bool,
        encode: impl FnOnce(&mut Vec<u8>),
    ) {
        let start = self.payload.len();
        encode(&mut self.payload);
        self.payload.resize(start + self.frame_bytes, 0);
        self.frames.push(AudioFrame {
            sample_index,
            source_index,
            discontinuity,
            start,
        });
    }

    /// Frames and their payloads, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&AudioFrame, &[u8])> {
        self.frames
            .iter()
            .map(|f| (f, &self.payload[f.start..f.start + self.frame_bytes]))
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// No frames.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Removes every frame, keeping capacity (the runtime, once it has published them).
    pub fn clear(&mut self) {
        self.frames.clear();
        self.payload.clear();
    }
}
