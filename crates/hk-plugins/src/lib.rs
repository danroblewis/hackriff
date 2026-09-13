//! hackriff plugin host. It runs decoder plugins (readsb, rtl_433, ...) as supervised
//! subprocesses described by manifests, with IPC for IQ/bits in and decodes out (C22, ADR-0003).
//! The process boundary is also the licence boundary that keeps GPL decoders out of the core
//! (ADR-0010). Core interface: changes are reviewed before merge.

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::DecodeId::new();
    }
}
