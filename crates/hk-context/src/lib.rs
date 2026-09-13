//! hackriff context. It keeps an offline-first cache of external feeds: space weather, gpsjam,
//! lightning, TLEs, SondeHub (C29, ADR-0008). It looks up known-signal priors such as band plans
//! and licence extracts, to separate known from unknown (C17). It correlates local Anomalies with
//! ExternalEvents into ranked Explanations for the attack map (C30).

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::ExplanationId::new();
    }
}
