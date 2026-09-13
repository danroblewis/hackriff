//! hackriff demodulation. Analog auto-mode AM/FM/WFM with estimated squelch and AGC, plus RDS
//! (C19). Own digital demodulators (FSK/PSK/QAM to soft symbols and bits, C20) feed Bitstreams
//! and decoder plugins. GPL decoders stay out of this crate, behind the plugin process boundary
//! (ADR-0010).

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::DemodulationId::new();
    }
}
