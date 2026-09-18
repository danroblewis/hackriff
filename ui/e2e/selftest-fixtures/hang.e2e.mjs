// A deliberately hanging spec (T-473). Used ONLY by `ui/e2e/selftest-timeout.mjs` to prove that
// `run.mjs`'s per-spec timeout actually fires, actually kills the whole process tree, and actually
// reports the spec red rather than letting a hang read as a pass.
//
// Lives outside `ui/e2e/` itself so the normal runner never finds it: `run.mjs` discovers specs with
// `readdirSync(HERE)` (non-recursive, `HERE` = `ui/e2e/`), which does not descend into this
// directory. The only way this file ever runs is `HK_E2E_EXTRA_SPECS`, an env var no ordinary
// invocation — `npm run e2e`, `just test-ui-e2e`, the gate — ever sets.
import { launch } from "../cdp.mjs";
import { startBackend } from "../backend.mjs";

// A real Chrome, parked doing nothing — the exact shape T-473 observed live (a browser launched,
// never navigated, never closed). We deliberately never call `kill()` on it: proving the RUNNER
// cleans it up is the point, not that this file behaves.
await launch({ headless: true });

// And a second `hk serve` of its own, the same shape `canvas-journey.e2e.mjs` uses — so this
// selftest proves both halves of the required cleanup (T-473 acceptance #2), not just the browser.
// Left running, on purpose, same as the Chrome above.
await startBackend();

// Hang. No timers, no I/O to wait on: `await new Promise(() => {})` never resolves, which is the
// simplest form of "a wait loop with no bound" the ticket names.
await new Promise(() => {});
