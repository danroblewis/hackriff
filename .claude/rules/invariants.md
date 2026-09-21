# Core invariants (placeholder — content moves here at the CLAUDE.md slim)

**Not yet wired.** This file reserves the home for the project's shared, role-agnostic invariants. Today they still live in the root `CLAUDE.md`, which every session and subagent inherits — so nothing is lost by this file being a stub.

When the supervisor does the reviewed **CLAUDE.md slim** (the second step of the orchestration migration), the following move *into* `.claude/rules/*.md` and get `@`-imported back into a lean root `CLAUDE.md`:

- **Signal & inventory model** (ADR-0017/0019): a signal is a time–frequency region `[start, end?]`, ongoing-until-ended, end revocable; inventory time-scoped to the view; Candidate/Confirmed/History; overlap is an error signal that triggers re-analysis; detection is fast, continuous, self-cleaning.
- **The view / canvas invariants**: one surface over one (time × frequency) window; one shared absolute-time axis; grey = genuinely unobserved (and it's the point); navigation = view arithmetic in time + a device command in frequency; the three honesty tiers (live-IQ detail / spectrum-history / survey-overview).
- **Live rendering, tile maintenance, playback**: tiles maintained **live and incrementally**, never gated on batch generation; live is the finest-level growing edge; the live/tuned tier renders at live-IQ FFT resolution (pyramid coarse only for zoom-out/history — T-484/T-501); spectrum trace viewport-wide + time-addressable.
- **Engineering**: the crate layout, the Rust-core / GPLv3-behind-the-plugin-boundary rule (ADR-0010), GPU Mac-first, the build/test/run commands.
- **Test strategy**: SigMF fixtures, e2e through the device interface (mock SDR behind the real interface), blind ground-truth (assert detection + top-k explanation, never lookup-and-tune).

The role files (`.claude/roles/`), agent files (`.claude/agents/`) and skills (`.claude/skills/`) already carry the **workflow** rules that used to bloat the shared CLAUDE.md — the point of the split is that a worker loads invariants + its role, not the coordination minutiae it never uses.
