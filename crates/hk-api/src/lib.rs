//! hackriff API. It carries the control plane used by the web UI and `hk` (live view and
//! inspector, C39) and the stream-output contract that delivers bits, symbols and decodes to
//! external programs with framing, backpressure and `content_class` gating (C24, ADR-0004).
//! Core interface: changes are reviewed before merge.
//!
//! - [`stream`]: re-export of the `hk-stream` crate, the versioned stream-output contract
//!   (`docs/stream-contract.md`). It lives in its own crate so `hk-plugins` can use it without
//!   depending on `hk-api`.

pub use hk_stream as stream;

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::BitstreamId::new();
    }
}
