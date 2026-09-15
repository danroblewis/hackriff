# MUI — Exploratory UI rewrite

Status: **brief for the coordinator** (2026-09-15, from the user via the supervisor). This is a new milestone. It was mocked up and deferred repeatedly but never planned; do it now. Read this, add MUI to `docs/11-roadmap.md`, add MUI tasks to `docs/tasks.yaml` with parallel_groups, and fan out. Runs in parallel with the tail of M2. Commit per task.

## Why

The current web UI is a thin client that has had M0b/M1/M2 panels **appended to the bottom** as they were built, so it stacks into a long scroll and doesn't match the product's exploratory intent. The user finds it hard to use. The backend is API-first (`docs/api.md` + contract tests from T-079), so the frontend can be replaced without touching the backend.

## The target

A **one-screen, exploratory UI** that fits without full-page scrolling and works down to phone width. The concrete design target is the mockup in the repo: **`ui/mockups/explorer-v3.html`** (open it — it is the spec, on example data). Build the real thing against the live API to match its layout and interactions.

Two modes via a top-bar toggle: **Explore** and **Decode**, an **Outputs dock** across the bottom visible in both, and an **always-on Capture timeline**.

### Explore
- **Left sidebar:** Signal inventory split into **Candidates** and **Confirmed** (per T-078), sortable, with **Promote / Delete** reachable without horizontal scroll; **Selections** below with their actions reachable. (Fixes the current sidebar button-overflow.)
- **Centre:** spectrum trace + WebGL waterfall; brackets mark confirmed/candidate/focused signals; DC masked; drag to select a region; hover readout.
- **Right focus panel:** the selected signal — big frequency, refined-from-output note, measurements, **ranked explanations** (suggestions, never truth), and actions (Listen, Decode/RDS, Record→Export clip, Stream out, Promote/Delete).

### Decode workbench
- **Pipelines** list (parallel decoders, incl. hop-following), the selected recipe's **stages**, and a blocks palette.
- **Per-stage plots** (MPX/subcarrier, eye, timing, constellation, sync search, group/frame counts, channel map).
- **Packet inspector** (the M1 inspector, given its proper home): frame list → hex+ASCII → layer tree, with **linked selection** both ways; shows the served byte-record and its TCP address.
- Stage **parameters** with blind-estimate "Use" suggestions and live quality (CRC/BCH pass rate, records/s, lock).

### Outputs dock
Every live stream (audio + decoded records) with meter/rate, mute, copy-address, stop. Listen **adds** a stream; a selection can Listen-to-all.

### Capture timeline
Always-on recording (no Record button); a scrubbable timeline; "reviewing N ago / LIVE".

## Constraints

- **Thin client.** No signal recognition/analysis/classification/demod/decode/parse logic in the UI — all of it stays in the backend behind `docs/api.md`. The UI only renders and calls the API. Add a CLAUDE.md rule if not already there.
- **One screen, responsive** to ~400px (phone); theme-aware; no full-page horizontal scroll.
- **Contract-tested:** rely on the T-079 API contract tests; add UI tests for the new components.
- **Reorganise, don't re-append:** the M1 inspector and M2 alarms/reports/scheduler panels move into their proper places (inspector→Decode; alarms/reports→Explore or a review surface), not stacked at the bottom.

## Suggested fan-out (coordinator finalises IDs/deps)

Do a design task first, then parallelise.
- **MUI-DESIGN (Opus high, core_interface):** component + state-store structure, the frontend↔API mapping for every panel, the build setup, and the framework decision (stay dependency-light — vanilla TS + a small reactive store, or a light lib like lit/Preact; justify). Produce a short ADR-0013. Blocks the rest. Keep the waterfall's existing WebGL renderer.
- Then parallel (mostly Sonnet, thin client over docs/api.md; waterfall/DSP-adjacent bits Opus):
  - App shell + mode toggle + Outputs dock + Capture timeline.
  - Explore: inventory (candidate/confirmed, sortable, reachable actions) + selections + focus panel + explanations.
  - Centre: spectrum + waterfall + brackets + region select (reuse existing WebGL).
  - Decode: pipelines + stages + per-stage plots.
  - Decode: packet inspector (reuse T-090) rehomed with linked selection.
  - Wire alarms/reports/scheduler into their places.
  - UI tests + phone-width pass; retire the old stacked layout.

Keep the demo mergeable; the supervisor rebuilds the bears demo when UI changes land.
