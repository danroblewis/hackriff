//! hackriff core sample path. It holds the source abstraction for HackRF and later SDRs behind
//! one trait (C01), sweep survey (C02), and dwell capture into a RAM ring buffer with pre-trigger
//! read (C03). It hosts the survey/dwell attention scheduler (C04, ADR-0005) and the SigMF
//! file-replay source that drives offline tests. Real-time path: no Python, no allocation in the
//! steady state. T-003 fills this in.

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        assert_eq!(hk_model::sigmf::Datatype::Ci8.bytes_per_sample(), 2);
    }
}
