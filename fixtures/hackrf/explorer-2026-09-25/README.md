# HackRF One fixtures, 2026-09-25 (explorer agent)

Three live captures clipped by the Mac-only explorer agent through `POST /api/iqbuffer/clip` on
its staging build, 2026-09-25, San Francisco (`~/.hackriff-ops/explorer/journal-20260925.md`): two
FM/RDS captures ~04:00-04:14 PDT, plus a P25 capture ~06:19 PDT (T-975, its own section below).
Hardware: HackRF One serial `0000000000000000d2b861dc263bc293`, firmware 2026.01.3, board rev
older than r6, libhackrf 0.9.2, antenna unknown (whatever was on the SMA, not changed). The FM/RDS
pair used LNA 32 / VGA 30 / amp on; bias-tee **untouched, so unknown** — the fixture's provenance
and `capture_settings` omit `bias_tee`/`antenna_port` rather than writing `off`/`unknown` literally
(`docs/sigmf-extension.md`: an absent key is unknown, and unknown is not `off`). All three clips
are 2.4 Msps x 5 s, ci8, 24 000 000 bytes (Git LFS). Licence: project-owned capture.

Regenerate the `hackriff:truth` annotations and manifest rows (data/meta files are placed by hand
from the explorer agent's `~/.hackriff-ops/explorer/captures/20260925/` output, which sits outside
the repo): `uv run --project py python py/fixtures/build_explorer_2026_09_25.py`.

Every emission annotation's `hackriff:truth.rds` is decoded **independently** by the oracle
`py/fixtures/rds_ref.py` run over this fixture's own 5 s window — not copied from the explorer
agent's live decode — and carries the explorer's original claim alongside it under
`rds.explorer_claim` plus an `rds.oracle_agrees_with_explorer_pi` flag. The pipeline under test
never sees any of this: the mock SDR replay source (`crates/hk-core/src/source/sigmf_replay.rs`)
reads only `global`/`captures` sample data and timing, never `annotations`, so `hackriff:truth` is
invisible to it exactly as for every other fixture.

| Fixture | Use cases | Truth summary (oracle-decoded) |
|---|---|---|
| `fm-101p3-pi1694` (capture centre 100.9 MHz) | SIGNAL-062 | **101.300 MHz WFM + RDS**, oracle PI `1694` (56/56 CRC-valid groups, BLER 0.0 over the 5 s window), PTY 7, TP 0, pilot 18999.890 Hz, clock −5.77 ppm. PS is dynamic (song/artist scroll); the window's only complete frame is `Animals ` (2x), consistent with the explorer's live RT "... Glass Animals - Heat Waves". Whole-file `overload` artefact: 11 811 945 / 12 000 000 samples clipped (98.4 %) at LNA 32/VGA 30/amp on — severe front-end overload in SF's FM environment, not a bug. |
| `fm-98p9-piA4FF` (capture centre 98.5 MHz) | SIGNAL-062 | **98.900 MHz WFM + RDS**, oracle PI `A4FF` (3/27 CRC-valid groups, BLER 0.639 — weak but internally consistent, all 3 groups vote the same PI), PTY 9, pilot 18999.849 Hz. **98.100 MHz WFM**, pilot present (18999.890 Hz) but 0 CRC-valid groups in the window: oracle agrees with the explorer's "no PI" claim. Whole-file `overload` artefact: 1 441 836 / 12 000 000 samples clipped (12.0 %). |

**Oracle cross-check (T-935): no disagreements.** The independent `rds_ref.py` oracle, run fresh
over each fixture's own 5 s window, confirms both PI codes the explorer agent's live RDS recipe
reported (`1694` and `A4FF`) and confirms that 98.1 MHz has a locked pilot but no decodable PI in
this short window, matching the explorer's own weaker-signal finding there (3 groups in 45 s live,
0 in this 5 s clip). Had the oracle disagreed with the explorer's truth, this README and the
fixture's `hackriff:truth` would record the disagreement rather than silently preferring one.

## The FLEX paging capture (T-949): external store, not this directory

A paging-band capture from the same session, `flex-pagers-930p8` (929-932 MHz, 12 s x 2.4 Msps),
got its use-case id from T-949 (`SIGNAL-088`, terrestrial FLEX/POCSAG paging) but is **not
committed under this directory**: at 57.6 MB the raw capture is over `fixtures/README.md`'s 25 MB
Git-LFS cap (three FLEX channels' frames are 1.875 s apart, so a 12 s clip was needed to see more
than one sync — see the journal's "Pagers" entry). It lives in the **external store**,
`fixtures/store/explorer-2026-09-25/flex-pagers-930p8.sigmf-{meta,data}` (gitignored, indexed by
the committed `fixtures/manifest.json`, `status: external`), built by the same
`py/fixtures/build_explorer_2026_09_25.py` this directory's FM/RDS pair uses.

Every emission's `hackriff:truth.paging` is decoded independently by the oracle
`py/fixtures/flex_ref.py` (FM discriminator -> 1600 Bd 2-level sync correlation against FLEX's
frame sync `0xA6C6AAAA`, both bit orders and polarities, then a 1600/3200 Bd re-slice of the data
following each sync to count clean FSK levels), not copied from the explorer's own
`tools/oracle_pager.py` claim, which is kept alongside each annotation under
`paging.explorer_claim`.

**Oracle cross-check (T-949): sync counts agree; level counts disagree on two of three channels.**
The oracle found the same number of frame syncs the explorer's own oracle claimed in this 12 s
clip at every channel — **3** at 929.6084 MHz, **1** at 929.9331 MHz, **2** at 931.1580 MHz (all
Hamming <= 3/32 of the exact sync word) — so no sync-count disagreement. Level counting is harder
on real, mostly-idle-between-frames air: at 931.1580 MHz (explorer: "2FSK") the oracle found a
clean 2-level split at both 1600 and 3200 Bd, agreeing; at 929.6084 and 929.9331 MHz (explorer:
4-level FSK payload) the oracle's re-slice found only 1-2 clean levels in the 2 s window following
each sync, not 4 — recorded as a **disagreement** in the fixture's truth and `manifest.json`
(`truth_summary`), not silently resolved either way. A 4-level channel with a short, low-duty-cycle
burst is a genuinely hard level-count case in 12 s of mostly-noise air; the disagreement is honest,
not a bug being hidden.

A fourth FLEX channel, 931.7331 MHz, was active in the explorer's wider 30 s live-detection window
but is idle in this 12 s clip (`hackriff:truth.paging.explorer_claim`, scenario annotation's
`not_in_clip_but_seen`) — recorded for context, not annotated as an emission since there is nothing
in this clip's samples to point at.

## The P25 C4FM capture (T-975): `p25-852p86-2p4M`

A third capture from the same explorer-agent session, taken later (2026-09-25 ~06:19 PDT, capture
centre 853.3 MHz, 5 s x 2.4 Msps, ci8, 24 000 000 bytes — under the 25 MB cap, so committed here
in Git LFS rather than the external store): `p25-852p86-2p4M.sigmf-{meta,data}`, use case
SIGNAL-085 (also SIGNAL-080, SIGNAL-087). Gain raised to LNA 40 / VGA 30 / amp on for this pass.
The companion 8 Msps wideband capture at the same nominal frequency (`p25-cc-852p86`, 80 MB) is
**not** committed (over the cap; not needed for this fixture's own truth).

Truth: an intermittent P25 C4FM emission at 852.8587 MHz. The explorer's own live app blind
detection found a candidate there (9.3 kHz, SNR 14.7 dB, modulation family "unknown" at
confidence 0.999 — expected, since P25/C4FM identification is not yet built) plus its own
`tools/oracle_p25.py` sign-sliced sync-correlation script, which claimed 4 P25 frame syncs
(`0x5575F5FF77FF`, <=1 sign error, 4800 Bd) in this clip and 0 on neighbouring channels. The
explorer's own re-grading — recorded in `hackriff:truth.p25.channel_classification`, as an
opinion with its reasoning, not as fact — is that this is **most likely a conventional or voice
P25 channel, not an established control channel**: the frame syncs are intermittent rather than a
control channel's near-continuous repeating TSBK stream, and the companion wideband capture at
this frequency did not itself resolve a confirmed control channel (7 passes / 0 confirmed / 0
TSBK). No TSBK/NID decode is attempted on this capture; it is blind-detection-and-identification
only.

`hackriff:truth.p25` is decoded **independently** by the oracle `py/fixtures/p25_ref.py`: FM
discriminator -> a fixed 4800 Bd symbol clock -> the frame sync's 24-symbol sign pattern (the
sync is sent using only the two outer C4FM deviation levels, so a plain sign slice recovers it) ->
a sliding 24-symbol correlation, tried across timing phase, symbol order and polarity, with a
**timing-phase-corroboration filter** (a merged hit must be seen at several nearby timing phases,
not just one, to count — this fixture's real bursts corroborated at 13-14 of 16 phases; the
strongest noise-floor coincidence on any of seven neighbouring, signal-free channels tried
corroborated at only 4) — not copied from the explorer's `tools/oracle_p25.py` claim, which is
kept alongside the annotation under `p25.explorer_claim`.

**Oracle cross-check (T-975): sync count disagrees by one; neighbours agree at zero.** The oracle
found **3** frame syncs in this 5 s clip (at 0.672 s, 2.584 s and 4.107 s; Hamming 2/1/1 out of 24
symbols) against the explorer's claimed 4 — recorded as a **disagreement** in the fixture's truth
and `manifest.json`, not silently resolved either way (the two oracles use different, independently
written correlation searches: the explorer's tries 6 timing phases at a single symbol order and
Hamming<=1 with no phase-corroboration filter; this one tries 16 phases across all four
order/polarity combinations with the corroboration filter above — a plausible source of a
one-event difference on a genuinely marginal signal, not investigated further here). The two
oracles agree exactly on the more decisive claim: **zero** frame syncs on every neighbouring
channel tried (seven, in this build), so a false-positive floor is not what is driving the count.

## Window 3: LMR/GMRS NBFM+CTCSS and conventional DMR (T-985)

Two more captures from the explorer agent's 2026-09-25 window-3 sweep of 460-470 MHz land-mobile/
GMRS (~06:37-07:03 PDT), taken through `POST /api/outputs/record/start` (whole tuned window)
rather than the clip endpoint the earlier three used, so their source `.sigmf-meta` carried no
`hackriff:clip_count` yet either (`build_explorer_2026_09_25.py` measures it directly, same gap
`build_p25` fixed). Same hardware as above; LNA 32 / VGA 30 / amp on, bias-tee **off** (confirmed
in this pass's provenance, unlike the FM/RDS pair's unknown).

- `lmr-461p125-nbfm-ctcss` (`fixtures/store/explorer-2026-09-25/`, **external**: SIGNAL-090):
  15 s x 2.4 Msps @ 461.675 MHz, 72 MB — over the 25 MB committed cap even before considering that
  the truth burst alone (8.1 s) would be ~39 MB, so it goes to the store whole, untrimmed, same as
  the FLEX capture above. Explorer's truth claims an 8.1 s NBFM burst at 461.125 MHz with a
  233.6 Hz CTCSS tone (measured 233.0 Hz ±1 Hz by the explorer's own `tools/nbfm3.py`, matched to
  the nearest standard tone), plus a continuous unidentified carrier at 461.9875 MHz and a brief
  (0.2 s in this file) NBFM burst at 462.225 MHz.

  **Oracle cross-check (T-985): CTCSS tone disagrees.** The independent oracle `ctcss_ref.py`
  (NBFM discriminator → zero-phase low-pass < 300 Hz → Welch-averaged tone estimate → nearest
  EIA 50-tone match) measured **100.0 Hz** (raw estimate 99.88 Hz, SNR 16.5 dB) over the claimed
  burst window `[6.9 s, 15.0 s)` — the burst's own timing (a clear +5.5 dB power step at 6.5-7 s,
  confirmed independently by direct power measurement, matching the claimed 6.9 s start to within
  the measurement's own smoothing window) is right, but the tone is not 233.6 Hz in this oracle's
  independent measurement. This is recorded as a **disagreement** in the fixture's truth and
  `manifest.json`, not silently resolved either way: swept across the whole channel in 5 kHz
  steps, the 100 Hz tone peaks sharply (16-18 dB) exactly at 461.120-461.125 MHz and nowhere else
  nearby, so this is a real, confidently-measured tone, just not the one claimed. At 462.225 MHz
  (the claimed 0.2 s burst) the oracle found no tone above its noise-guard threshold over the whole
  15 s file, consistent with the explorer's own "not in this file" note (no disagreement, since no
  same-file claim exists to check against).

- `dmr-464p6125-bs` (`fixtures/hackrf/explorer-2026-09-25/`, **committed**, Git LFS: SIGNAL-091):
  the raw capture is 10 s x 2.4 Msps @ 464.3 MHz, 48 MB — over the cap, but the independent oracle
  found the *entire* frame-sync burst sits inside the file's final ~1.25 s (12% duty over the full
  10 s), so a **5 s tail trim** (`[5.0 s, 10.0 s)`, `py/fixtures/trim.py`) keeps 100% of the real
  signal content at exactly 24 000 000 bytes, under the cap. The untrimmed original is kept at
  `fixtures/store/explorer-2026-09-25/dmr-464p6125-bs-full.sigmf-{meta,data}` (external), and the
  committed clip's manifest row carries `source`/`source_sha256`/`source_start_s` pointing at it
  (`fixtures/README.md`'s convention, same as `py/fixtures/build_2026_09_13.py`'s trimmed
  fixtures). Explorer's truth claims a 4-level-FSK bursty channel at 464.6125 MHz with a DMR
  BS-sourced DATA sync found 42x over the whole 10 s file, plus two continuous unidentified
  carriers and an NBFM burst at 464.700 MHz with a 100 Hz tone "measured at 06:38, not re-verified
  in this file".

  **Oracle cross-check (T-985): sync count disagrees by one.** The independent oracle `dmr_ref.py`
  (FM discriminator → 4800 Bd 4-level-sliced 48-bit ETSI SYNC-pattern correlation, tried across
  both bit orders/polarities and all four standard SYNC patterns, with the same timing-phase
  corroboration discipline as `p25_ref.py`'s intermittent-burst handling) found **41** `BS_data`
  syncs in the trimmed 5 s clip against the explorer's claimed 42 over the full 10 s file —
  recorded as a **disagreement**, not silently resolved (a plausible one-event difference between
  two independently written correlation searches on a genuinely marginal signal, the same
  character as the P25 capture's own off-by-one above). The 100 Hz tone at 464.700 MHz was not
  re-measured as matching or disagreeing with the explorer's claim, since that claim is explicitly
  for an earlier, wider observation window and not asserted for this specific clip.

