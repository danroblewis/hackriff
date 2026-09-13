# ADR-0008 — Offline-first external context

**Status:** PROVISIONAL
**Touches:** C17, C29, C30; ExternalEvent/Explanation ([docs/07 §2.17–2.19](../07-data-model.md))

## Context

The attack map works from this device's own survey history plus external context feeds whenever connectivity exists; the device must be offline-first (CLAUDE.md). Feeds include NOAA SWPC scales/GOES/Kp/solar wind, lightning networks, TLEs + pass predictions, SondeHub launches, GPS-jam indices, tropo/Es forecasts, PSKReporter/WSPR spots, FMLIST, DSN schedules. Reference priors include 47 CFR §2.106 band plans, FCC ULS extracts, sigidwiki/Artemis, RadioReference ([docs/04 §1.1](../04-radio-engineering-and-signals-analysis.md)).

## Decision (provisional)

- **Everything works from a local cache.** Feeds and reference data are fetched opportunistically when online and stored as ExternalEvents (time-bounded) or reference tables (band plan parsed once to a compact table; ULS with a spatial index; TLEs; sigidwiki DB). No feature blocks on the network.
- **Two roles, one cache.** C29 fetches and caches; C17 is the query/prior interface over the cache. FMLIST/SatNOGS appear in both roles by design (docs/06 §5).
- **Satellite passes are computed locally** from cached TLEs (C29), so pass windows exist offline for the scheduler and for correlation (docs/06 §5).
- **Cache freshness is explicit:** every ExternalEvent and reference table carries `fetch_time`/validity; the UI and correlation show cache age; stale data degrades gracefully (a correlation says "based on data N days old").
- **Sync/export:** when online, pull deltas (ULS weekly, TLEs daily, SWPC frequently) on a budget; export own survey/inventory for sharing. Nothing in the design rules out later sharing (CLAUDE.md), but the device is not a network node.
- **Bundle a seed cache** at build/first-run (band plan, a regional ULS slice, a TLE snapshot, the sigidwiki DB) so a brand-new offline device is already useful.

## Consequences

- Correlation and priors are deterministic given a frozen cache — which is exactly how they are tested with no network in CI ([docs/07 §2.17](../07-data-model.md), [docs/10](../10-test-strategy.md)).
- Storage cost is small and bounded; the ULS spatial index is the largest item and is regional.
- Licence/ToS of each feed must be checked before bundling or redistributing (tracked in [ADR-0010](0010-language-and-licence-ledger.md)); RadioReference requires per-user credentials ([docs/04 §1.1](../04-radio-engineering-and-signals-analysis.md)).
