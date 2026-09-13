//! hackriff signal characterisation. Parameter estimation covers occupied bandwidth, CFO and
//! SNR (C13). Blind symbol estimation covers symbol rate, modulation order, deviation and
//! roll-off, with confidence (C14). Estimates feed demodulation so parameters are never picked
//! by hand.

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::EmitterId::new();
    }
}
