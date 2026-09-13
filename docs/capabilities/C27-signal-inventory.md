# C27 · signal-inventory
> Layer E — Remember · Status: taxonomy frozen 2026-09-13 (resolved in docs/06 §5) · Depends on: C05, C06, C10, C15, C17, C18, C22, C25, C28 · Used by: C17, C24, C30, C39

## Purpose
The persistent emitter database. For each emission cluster the device has seen it records when and where, what it looks like, what it is believed to be and how sure, with links to recordings, tracks and explanations. It turns waterfall pixels into objects the user can list, name, tag and revisit (docs/03 §5.1 #1, #4). Serves workflow steps 3–4 and feeds "new emitter" anomalies to the attack map. It fills a gap no open-source tool covers (docs/03 §6, gap 2). Layer E is loops, not a line (docs/06 §2.1/§5): C27↔C30 (the inventory feeds correlation; correlation writes explanations back) and C27↔C17 (own history is a prior; priors write expected/unexpected status back).

## Interface
- **Inputs:**
  - Tracks with timing features (C10).
  - Fingerprints/cluster ids (C18).
  - Class distributions incl. `unknown` (C15).
  - Decoded identities and CRC-valid frames (C22).
  - Expected/unexpected priors (C17).
  - Labels (C28), suspect flags (C05), position (C06).
  - Recording ids (C25) and explanation ids (C30).
- **Entities (provisional):**
  - `Emitter`: id (UUID), status {candidate, suspect-artifact, unknown, identified}, expected flag, centre, bandwidth, raster offset, family/protocol + confidence, fingerprint, decoded identity, first/last seen, sites, tags, notes.
  - `Sighting`: hourly aggregate per emitter per site. Count, on-time, duty cycle, periodicity, peak/median SNR and RSSI, gain state.
  - `IdentityClaim`: source {decoder, user, classifier, prior}, value, confidence, algorithm/model version, time.
  - Links from each Emitter to its Recordings, Tracks, Explanations and Annotations.
- **Queries:** frequency range, time window, site/bbox, status, tag, protocol, "new since T", "not seen since T", "unexpected here". Sort by interestingness score (docs/04 §2).
- **Events:** new emitter, emitter returned, identity changed, unknown recorded (notification list in docs/04 §11.2).
- **Sizes (estimate):** ~1 kB per Emitter row and ~100 B per Sighting. Worst case 10k emitters × 24 sightings/day ≈ 24 MB/day, so prune idle emitters' sightings.

## Methods
- **Store:** SQLite (docs/06 "SQLite-class"; docs/03 §7; docs/04 §11.2), WAL mode. Index (f_lo, f_hi), last_seen and status; an R-tree over frequency×time is an option.
- **Entity resolution:**
  - Match a new track to an existing emitter by fingerprint: raster, OBW, family, levels, symbol rate, sync word, timing, hop set, CRC (docs/04 §7.6). Then frequency tolerance, then site.
  - Cluster unknowns with DBSCAN on normalised features (docs/04 §7.7 step 1).
  - Keep merge/split history so mistakes can be undone.
- **Identity precedence:** decoder lock/CRC-valid > user label > classifier > prior. Trial decoding is the final arbiter (docs/03 §7). Store every claim and derive the displayed identity.
- **Known vs unknown:** fuse with C17 using λ0 > 0 so non-compliant or unknown emitters never get zero probability (docs/04 §1.1).
- **Artifact gate:** clipping, spur-mask or image flags keep an entry `suspect-artifact` until the retune and gain-step tests pass (docs/04 §10.3–10.4).
- **Separate measurement from interpretation:** re-run classifiers over stored records as models improve (docs/04 §11.2).

## Platform constraints
- **Writes, not compute:** compute is negligible (docs/06), but small writes are many. Batch Sightings, and use one writer to avoid SQLite lock contention with the C25 index.
- **Battery pulls:** survive power loss mid-transaction with WAL plus a checkpoint on low battery.
- **Offline, shareable later:** fully offline. UUIDs rather than autoincrement keep later export/sync possible (CLAUDE.md: don't rule out sharing).
- **Clock/position quality** (C06) bounds first/last-seen accuracy and site assignment.

## Prior art and reuse
- **Signal-table precedents** (commercial): Aaronia IQ Pulse Inspector's table of found signals, CRFS DeepView Signal Discovery, OmniSIG JSON/Elastic output (docs/03 §3.9, §5.2).
- **Kestrel TSCM:** survey → compare-with-baseline workflow (docs/03 §3.8).
- **Artemis / sigidwiki:** offline SQLite reference DB with frequency, bandwidth, mode and ACF. A reference, not a live inventory; v4.2.0, active (docs/03 §3.7). Licence: check.
- **Partial precedents:** Trunk Recorder call logs, OpenWebRX+ auto bookmarks, Spectre SQL power history (docs/03 §6 table).
- **rtl_433 flex specs:** the fingerprint model to generalise (docs/06 C18).

## Pitfalls
- **Ghost emitters:** IMD, images and spurs flood the inventory in cities (docs/02 §1.7).
- **Over-splitting:** one transmitter becomes many entries through CFO drift, Doppler or gain changes.
- **Over-merging:** every 433.92 MHz OOK device collapses into one entry.
- **Moving targets vs a moving device:** the same aircraft appears at many sites; a fixed tower looks "new" at every new site.
- **Multi-channel systems:** hoppers and trunked systems span many channels as one logical system (docs/04 §4.7, §8.4).
- **Overconfident classifiers** mark unknowns as identified (docs/04 §5.4 #4).
- **Legal:** encrypted traffic is metadata only. Never store cellular or common-carrier paging contents as "identities" (docs/04 §1.3, §8.3).
- **Unbounded growth** from transient noise-like candidates: expire unconfirmed ones.

## Testing
- **Scenario replay:** SigMF fixture with scripted emitters: a periodic 433 MHz OOK sensor, an FSK beacon, a hopper, and an IM3 product from two strong tones. Assert:
  - exactly the expected emitters;
  - correct first/last seen;
  - duty cycle within tolerance;
  - the IM3 entry is `suspect-artifact`.
- **Idempotence:** replaying twice creates no duplicate emitters.
- **Precedence:** conflicting classifier and decoder claims resolve to the decoder, and both are stored.
- **Queries:** "new since T", frequency-range and status filters return the expected sets.
- **Undo:** reversing a merge restores the links.
- **Needs hardware:** real urban IMD population, multi-day growth.

## Example use cases
Regenerated from `use-cases.yaml` (primary, then notable secondary):
- AWARE-066 — Legacy network sunset tracker
- SIGNAL-021 — (signal-inventory primary)
- SIGNAL-037 — (signal-inventory primary)
- AWARE-007 — (signal-inventory secondary)
- AWARE-013 — (signal-inventory secondary)
- AWARE-016 — GSM broadcast channel inventory
- AWARE-024 — (signal-inventory secondary)
- AWARE-053 — (signal-inventory secondary)
- SIGNAL-035 — (signal-inventory secondary)
- SIGNAL-046 — (signal-inventory secondary)

## Open questions
- **Entity levels:** docs/06 says "one entry per emission cluster". Decoded identities (ICAO, MMSI, sensor ID) and logical systems (trunk site, hop set) may need their own levels.
- **Global or per-site emitters** for a handheld?
- **Suspect artifacts:** store them in the inventory, or in a quarantine table?
- **Dependency sketch (resolved, docs/06 §2.1/§5):** Layer E is loops — C27↔C30 (explanation links) and C27↔C17 (own history as a prior) — now shown in §2.1. These are runtime data links, not build dependencies.
- **Retention:** raw Detections vs aggregates only.

## Reading list
1. docs/03 §5.1 "Concrete problems" (#4)
2. docs/04 §11.2 "Mapping to an exploration device"
3. docs/04 §7.6 "Protocol fingerprinting"
4. docs/04 §1.1 "Allocation vs. assignment vs. actual use" (prior formula)
5. docs/04 §10.3 "Spur identification and removal"
6. docs/03 §5.2 "Best UX ideas that already exist (steal these)"
