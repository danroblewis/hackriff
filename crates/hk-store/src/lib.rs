//! hackriff persistence outside the relational store. Covers the multi-resolution spectrum-history
//! pyramid of SpectrumTiles that answers "what has this region looked like over time" (C26),
//! SigMF recording with pre-trigger IQ and embedded annotations (C25), and the disk quota and
//! retention policy (ADR-0006).

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::RecordingId::new();
    }
}
