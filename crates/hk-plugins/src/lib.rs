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
//! - [`output`]: the stdout NDJSON message plane, parsed into `Decode`/`Annotation` under the
//!   ceiling `clamp(manifest class, input channel class)`, with the manifest's metadata allowlist
//!   (the shared [`hk_stream::policy`] model), the `sample_index` input bound and confidence
//!   rounding applied whenever a line's class forbids content.
//! - [`ingest`]: [`Ingest`], which stores through the `Repository` (gated content is stored
//!   metadata-only; restricted rows that skipped the policy are stripped) and republishes on a
//!   T-016 messages stream.
//!
//! The test plugin is the `hk-dummy-plugin` binary (`plugins/dummy/manifest.json`).

pub mod host;
pub mod ingest;
pub mod manifest;
pub mod output;

pub use host::{
    HangBudget, HostError, InputStreamDesc, LogTail, PluginContext, PluginInstance, PluginMonitor,
    PluginState, PluginStats, PushOutcome,
};
pub use ingest::{Ingest, IngestStats, Stored};
pub use manifest::{
    Charset, EXAMPLE_RESTRICTED_PAGING_OUTPUT, HzRange, IdentitySpec, InputKind, InputSpec,
    MANIFEST_VERSION, MAX_ALLOWLIST_LEN, ManifestError, MetadataPolicy, MetadataType, OutputSpec,
    PluginManifest, RESTRICTED_DEFAULT_MAX_LEN, ResourceLimits, RestartPolicy,
    output_metadata_policy,
};
pub use output::{
    Parsed, PluginOutput, SAMPLE_INDEX_OUT_OF_RANGE, parse_line, resolve_class, sanitize_metadata,
};

#[cfg(test)]
mod tests {
    #[test]
    fn links_against_the_model() {
        let _ = hk_model::DecodeId::new();
    }
}
