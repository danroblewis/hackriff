//! hackriff detection. CFAR plus spectral-kurtosis detection over spectrum frames (C09), with
//! spur/image/clip flags taken from Provenance and the SpurMask. Burst records are linked into
//! Tracks with period, duty cycle and hop features (C10). Produces the immutable Detection
//! objects of docs/07 §2.9–2.10.

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::DetectionId::new();
    }
}
