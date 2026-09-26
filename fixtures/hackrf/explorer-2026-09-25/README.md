# HackRF One fixtures, 2026-09-25 (explorer agent)

Four live captures clipped by the Mac-only explorer agent through `POST /api/iqbuffer/clip` on its
staging build, 2026-09-25 ~04:00-04:58 PDT, San Francisco (`~/.hackriff-ops/explorer/journal-20260925.md`).
Hardware: HackRF One serial `0000000000000000d2b861dc263bc293`, firmware 2026.01.3, board rev
older than r6, libhackrf 0.9.2, antenna unknown (whatever was on the SMA, not changed). LNA 32 /
VGA 30 / amp on for all four; bias-tee is **untouched, so unknown** for the first pair (T-935) — the
fixture's provenance and `capture_settings` omit `bias_tee`/`antenna_port` rather than writing
`off`/`unknown` literally (`docs/sigmf-extension.md`: an absent key is unknown, and unknown is not
`off`) — and recorded as `"off"` for the second pair (T-960), per the explorer's own truth claim.
All four clips are 2.4 Msps x 5 s, ci8, 24 000 000 bytes (Git LFS). Licence: project-owned capture.

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
| `fm-88p5-pi3AAB` (capture centre 89.0 MHz) | SIGNAL-062 | **88.500 MHz WFM + RDS**, oracle PI `3AAB` (4/34 CRC-valid groups, BLER 0.507 — weak), PTY 22, TP 0, pilot 18999.665 Hz, clock −17.62 ppm. **89.435 MHz WFM**, pilot present (18999.935 Hz) but 0 CRC-valid groups: oracle agrees with the explorer's "no PI, weak" claim. Whole-file `overload`: 579 551 / 12 000 000 samples clipped (4.8 %), consistent with the explorer's `app_overload_flag: true`. |
| `fm-106p1-pi1323` (capture centre 106.5 MHz) | SIGNAL-062 | **106.100 MHz WFM + RDS**, oracle PI `1323` (22/55 CRC-valid groups, BLER 0.227), PTY 16, TP 0, pilot 18999.894 Hz, clock −5.60 ppm. **106.907 MHz WFM**, pilot present (18999.114 Hz) but 0 CRC-valid groups: oracle agrees with the explorer's "pilot lock, no RDS" claim. Whole-file `overload`: 694 878 / 12 000 000 samples clipped (5.8 %), consistent with the explorer's `app_overload_flag: true`. |

**Oracle cross-check (T-935): no disagreements.** The independent `rds_ref.py` oracle, run fresh
over each fixture's own 5 s window, confirms both PI codes the explorer agent's live RDS recipe
reported (`1694` and `A4FF`) and confirms that 98.1 MHz has a locked pilot but no decodable PI in
this short window, matching the explorer's own weaker-signal finding there (3 groups in 45 s live,
0 in this 5 s clip). Had the oracle disagreed with the explorer's truth, this README and the
fixture's `hackriff:truth` would record the disagreement rather than silently preferring one.

**Oracle cross-check (T-960): PI agrees on both; one vote-count discrepancy, not a PI
disagreement.** `rds_ref.py` re-run fresh over each fixture's own 5 s window confirms `3AAB` at
88.5 MHz (the explorer's own truth file already logged the oracle's `x4` vote there, and this
build's independent run agrees: 4) and `1323` at 106.1 MHz (PI agrees), but the **vote count and
BLER differ from the explorer's narrative claim of "x18 votes, block error rate 0.29"** — this
build's fresh run found **22 votes, BLER 0.227**. The PI itself is not in question (both runs
decode `1323`); the difference is in exactly how many of the window's groups landed on a CRC-valid
frame, which can shift slightly between runs of the same decoder over the same 5 s buffer depending
on lattice-sync recovery order. Recorded here rather than silently reconciled, per the "never edit
the truth silently" rule; the committed `hackriff:truth.rds.pi_votes` carries this build's fresh
count (22), not the explorer's narrative figure. Both new stations' 89.435/106.907 MHz "no RDS"
claims are confirmed by the oracle exactly as the FM/RDS pair above.

The T-960 pair's raw `.sigmf-meta` (a slightly different staging-build capture path than T-935's)
omitted `hackriff:clip_count` per capture entirely rather than writing `0`; the build script
backfills it from the sample data itself (`fxlib.count_clipped`), which is how the real 4.8 %/
5.8 % clip fractions above were found — the naive "missing key defaults to 0" reading would have
under-reported both as unclipped, contradicting the explorer's own `app_overload_flag: true`.

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

**Settled by the decoder (T-950): every channel's data is 4-level.** `hk_demod::flex` decodes all
six syncs in this clip as frames whose frame information word is BCH(31,21)-valid, and at each it
re-slices the frame's data under every (rate, level-count) hypothesis FLEX defines and keeps the
one whose codewords check. The frame information word's own mode declaration and that measurement
agree at every frame, and the level choice was decisive at every frame:

| Channel | Frames | Declared / measured | Clean data codewords |
|---|---|---|---|
| 929.6084 MHz | 3 (cycle 8, frames 7, 8, 13) | 3200 Bd 4-level (6400 bit/s) | 61/64, 14/16, 62/64 |
| 929.9331 MHz | 1 (frame 12) | 1600 Bd 4-level (3200 bit/s) | 16/16 |
| 931.1580 MHz | 2 (frames 10, 11) | 3200 Bd 4-level (6400 bit/s) | 170/176, 15/16 |

Outer deviation measured 4.48-4.57 kHz on all three (FLEX: +/-4.8 kHz). So the explorer was right
at 929.6084 and 929.9331 MHz and wrong at 931.1580 MHz, and the oracle's 1-2 levels were the
**header**: sync-1 and the frame information word are always 2-level at 1600 Bd, a 2 s re-slice
after a sync on a channel that sends one or two 160 ms blocks is mostly header and idle, and a
2-level histogram there is what a 4-level FLEX frame looks like. The truth annotations are left as
T-949 wrote them (the acceptance member `captured_flex` asserts the level count the codewords
chose, not either claim); this paragraph is the resolution.

A fourth FLEX channel, 931.7331 MHz, was active in the explorer's wider 30 s live-detection window
but is idle in this 12 s clip (`hackriff:truth.paging.explorer_claim`, scenario annotation's
`not_in_clip_but_seen`) — recorded for context, not annotated as an emission since there is nothing
in this clip's samples to point at.
