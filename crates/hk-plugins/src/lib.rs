//! hackriff plugin host. It runs decoder plugins (readsb, rtl_433, ...) as supervised
//! subprocesses described by manifests, with IPC for IQ/bits in and decodes out (C22, ADR-0003).
//! The process boundary is also the licence boundary that keeps GPL decoders out of the core
//! (ADR-0010). Core interface: changes are reviewed before merge.
//!
//! Spec: `docs/stream-contract.md` §9.
//!
//! - [`manifest`]: the JSON [`PluginManifest`] (licence and content class required).
//! - [`host`]: [`PluginInstance`], which spawns, feeds (drop-not-block), supervises (restart with
//!   backoff, crash-loop cap, hang watchdog) and kills one plugin process.
//! - [`output`]: the stdout NDJSON message plane, parsed into `Decode`/`Annotation` with the
//!   manifest class as a ceiling.
//! - [`ingest`]: [`Ingest`], which stores through the `Repository` (gated content is stored
//!   metadata-only) and republishes on a T-016 messages stream.
//!
//! The test plugin is the `hk-dummy-plugin` binary (`plugins/dummy/manifest.json`).

pub mod host;
pub mod ingest;
pub mod manifest;
pub mod output;

pub use host::{
    HostError, InputStreamDesc, PluginContext, PluginInstance, PluginMonitor, PluginState,
    PluginStats, PushOutcome,
};
pub use ingest::{Ingest, IngestStats, Stored};
pub use manifest::{
    HzRange, InputKind, InputSpec, MANIFEST_VERSION, ManifestError, OutputSpec, PluginManifest,
    ResourceLimits, RestartPolicy,
};
pub use output::{Parsed, PluginOutput, parse_line, resolve_class};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::DecodeId::new();
    }
}
