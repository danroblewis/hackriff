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
