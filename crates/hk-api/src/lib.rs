//! hackriff API. It carries the control plane used by the web UI and `hk` (live view and
//! inspector, C39) and the stream-output contract that delivers bits, symbols and decodes to
//! external programs with framing, backpressure and `content_class` gating (C24, ADR-0004).
//! Core interface: changes are reviewed before merge.
//!
//! - [`stream`]: the versioned stream-output contract (`docs/stream-contract.md`): framing codec,
//!   stream header, records, egress gating, the drop-not-block [`stream::Publisher`], UDS/TCP
//!   listeners, a reference reader, and the [`stream::DecoderFeed`] data plane used by the plugin
//!   host (T-014).

pub mod stream;

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::BitstreamId::new();
    }
}
