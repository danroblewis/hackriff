# C29 · context-feeds
> Layer F — Explain · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C06 · Used by: C04, C30, C34, C39

## Purpose
Keeps a local, typed, time-stamped store of external events that can explain local spectrum changes (feeds listed below). It syncs whenever connectivity exists, and everything downstream must work from a stale cache (CLAUDE.md offline-first rule). It is the external half of the attack map (docs/05 §3, AWARE-044).

C29 also **computes satellite passes from cached TLEs**: TLE propagation is a feed-side computation, not its own capability (docs/06 §5). C34 (doppler-tracking) consumes those passes, C30 (event-correlation) uses the pass windows, and C04 (attention-scheduler) can dwell on predicted passes/launches (docs/06 §2.1). FMLIST and the SatNOGS DB appear in both C29 and C17: C29 fetches and caches them, while C17 is the query/prior interface over the cache — same data, two roles (docs/06 §5).

## Interface
- **`ExternalEvent`** (provisional):
  - feed id, event type;
  - t_start/t_end (UTC), t_published, t_fetched;
  - geometry {global | point+radius | polygon/hex cell | track | orbit};
  - affected frequency range, if implied; magnitude/scale;
  - raw payload ref, parser version.
- **`FeedState`:** last attempt, last success, cache age, coverage intervals (spans known complete), last error, terms note.
- **`ReferenceSnapshot`:** non-event data (TLE set, transmitter list) with epoch and age.
- **Queries:**
  - `events(type?, t0, t1, bbox?)`;
  - `coverage(feed, t0, t1)`, so C30 can tell "no event" from "no data";
  - `freshness()`.
- **Config:** enabled feeds, poll interval, geo-filter radius around the site, cache bytes/age limits, sync policy (Wi-Fi only, external power only), credentials.
- **Feeds named in docs/05.** Access method, cadence, history depth and terms are **unverified** for all of them:

| Feed | Event type | Use cases |
|---|---|---|
| NOAA SWPC: R/S/G scales, GOES X-ray, Kp, solar wind, D-RAP | flare, storm, absorption | SPACE-012, SPACE-015, SPACE-032 |
| Blitzortung | lightning strokes | AWARE-032, SPACE-044 |
| gpsjam.org | daily GNSS-degradation cells | AWARE-001, AWARE-006 |
| CelesTrak TLEs; SatNOGS Network/DB | orbits, passes, transmitters | AWARE-064, SIGNAL-035 |
| SondeHub | launches, tracks | AWARE-062, SIGNAL-074 |
| PSKReporter; WSPRnet/wspr.live | spots → band openings | PROP-004, PROP-001, AWARE-059 |
| Tropo/Es forecasts (dxinfocentre, DXMaps, MMMonVHF) | ducting, Es | AWARE-060, PROP-023 |
| FMLIST | transmitter list | SIGNAL-068 |
| NASA DSN Now | spacecraft/antenna schedule | SPACE-079 |
| HAARP campaigns, STEREO beacon, FCC DIRS, RSTN/e-CALLISTO | misc | AWARE-044, SPACE-080, AWARE-068, SPACE-005 |

Only RadioReference has documented terms (SOAP API; each end user needs RR Premium, docs/04 §1.1); it belongs to C17.

## Methods
- **One adapter per feed:** fetch → store raw payload immutably → parse into ExternalEvents → update coverage. This is orchestration, so Python is acceptable (CLAUDE.md).
- **Classify feeds by connectivity need:**
  - **Offline-computable:** satellite passes from cached TLEs (SGP4), sun position and greyline, synoptic radiosonde windows at 00Z/12Z (AWARE-062).
  - **Slow reference:** TLEs, FMLIST, SatNOGS DB. Refresh when online and expose age.
  - **Near-real-time:** SWPC, lightning, spots.
  - **Post-hoc:** gpsjam daily, outage reports.
- **Backfill** offline periods on reconnect so C30 can explain retroactively; record gaps.
- **Filter at ingest** by geography (e.g., lightning within a radius) and by band (spots on bands the device watched) to bound storage.
- **Network hygiene:** conditional requests and backoff; never block capture. Allow manual import of feed snapshots from removable media.
- **Time:** normalise to UTC, and record source precision (daily, minute, ms).

## Platform constraints
- **Connectivity:** intermittent only; no device-to-device cellular linking (CLAUDE.md scope).
- **Power:** network radios draw from a 15–38 W Tier B budget (docs/02 §7.2). Sync on external power or on a schedule.
- **Disk:** cap raw payload retention; parsed events are bytes–kB each (estimate), lightning dominates volume.
- **Clock:** the device clock must be GNSS-disciplined (C06; docs/02 §1.8), or joins against feeds shift.
- **TLE age:** accuracy degrades as TLEs age (magnitude unverified), so pass predictions carry the TLE epoch. *Implemented (T-276):* `hk_context::passes` (near-earth SGP4, checked against the published verification vectors; SDP4/deep space is refused, not approximated) predicts AOS/TCA/LOS from the cached `celestrak-tle` snapshot (`feeds::tle`). Every pass carries its TLE epoch and age; the planner widens the timing margin with age, marks sets older than 14 days `Stale` and does not reserve them by default, and reports the feed's cache age and failed refreshes beside the plan.
- **GNSS orbit references (T-324, SIGNAL-032):** IGS precise orbits (SP3, `igs-sp3`) and IGS merged broadcast ephemerides (RINEX nav, `igs-brdc`) are reference snapshots through the same cache, `refresh` and `FeedFetcher` seam as the TLE feed (`feeds::gnss_orbits`), not a second feed path. `compare_from_cache` checks received GPS ephemerides against them (`hk_context::ephemeris`). An uncached reference is `NotYetFetched` (with the last failed attempt), a cached one that does not span the ephemeris is `NotCovered`, and one lacking the satellite is `NoReferenceForSv`; none of them is agreement. Access details and tolerances (25 m, 60 ns) are unverified against a real IGS pair.

## Prior art and reuse
- **OpenWebRX+:** background decoding that uploads spots to PSKReporter, APRS-IS and WSPRnet; Daylight scheduler (docs/03 §2.4).
- **SatDump** pass-driven auto scheduler (docs/03 §3.6); **SDRangel** Satellite Tracker and SID plugins (docs/03 §2.2).
- **gpsjam method:** jamming inferred from ADS-B NACp (AWARE-001). The device can compute its own version through C22.
- **Feed licences/terms:** check. The docs state none except RadioReference.

## Pitfalls
- **Silent adapter breakage:** feed outages, API or ToS changes and rate limits. Alert on parse failures and stale coverage.
- **Stale cache presented as current:** every consumer must see the cache age.
- **"No event cached" ≠ "no event happened":** check coverage.
- **Coarse products joined as precise:** a daily gpsjam cell is not a timestamp.
- **Terms vs sharing:** terms that forbid redistribution or commercial use conflict with future export (lightning networks suspected; unverified).
- **Privacy:** uploading own spots or sonde reports reveals location. Make uploads opt-in (C24).

## Testing
- **Parsers:** a recorded raw-payload snapshot fixture per feed with expected events. A schema-changed fixture must fail loudly.
- **Offline simulation:** disable the network for a scripted 3 days, then restore it. Assert age is reported during the outage and backfill completes coverage.
- **Pass prediction:** a TLE fixture gives pass times matching a reference propagator within tolerance.
- **Backoff:** a mock server returning 429/5xx.
- **Needs network:** scheduled adapter smoke tests outside CI; a terms review per feed.

## Example use cases
Regenerated from `use-cases.yaml`:
- SPACE-005 — (context-feeds primary)
- SPACE-020 — (context-feeds primary)
- SPACE-021 — (context-feeds primary)
- SPACE-022 — (context-feeds primary)
- SPACE-030 — (context-feeds primary)
- SPACE-033 — (context-feeds primary)
- PROP-001 — Beacon-network propagation baselines
- PROP-004 — WSPR/PSKReporter band-opening spots
- AWARE-006 — GNSS jamming timeline vs. geopolitical events
- AWARE-039 — (context-feeds primary)

## Open questions
- **Unverified access:** endpoints, cadence, history and terms for every feed need a spike (not web-verified).
- **Overlap with C17 (resolved, docs/06 §5):** FMLIST/SatNOGS DB are the same data in two roles — C29 fetches and caches; C17 is the query/prior interface over the cache.
- **Ephemerides owner (resolved, docs/06 §5):** C29 computes passes from cached TLEs; C34 consumes them; C30 uses pass windows.
- **Missing edge (resolved, docs/06 §2.1):** C29 → C04 (dwell on predicted passes/launches) is now an edge.
- **Connectivity:** is Wi-Fi or phone tethering acceptable? CLAUDE.md only excludes device-to-device cellular links.
- **Uploads:** do PSKReporter/SondeHub contributions belong to C24 or C29? (Provisional; see the Phase 3 stream-contract ADR for C24's egress role.)

## Reading list
1. docs/05 §3 "Spectrum Situational Awareness, Interference & Anomalies" (subsections "Space Weather, Propagation & Natural Explainers", "Satellite & Space-Segment Interference")
2. docs/06 §2 "Capability taxonomy (39 capabilities)" (C17, C29, C30 rows)
3. docs/05 §1 "Space Weather, the Sun, Radio Astronomy & Natural Radio" ("Solar Activity & Space Weather")
4. docs/04 §1.1 "Allocation vs. assignment vs. actual use" (machine-readable access paths)
5. docs/03 §2.4 "Web-based and embedded receivers" (OpenWebRX+ background decoding)
6. docs/03 §3.6 "Satellites"
