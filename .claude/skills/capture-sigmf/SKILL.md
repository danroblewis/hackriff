---
description: Capture and annotate a real SDR recording as a SigMF fixture with its settings. Use for fixture capture and HIL. Receive-only, one radio at a time, record every setting.
disable-model-invocation: false
allowed-tools: Bash, Read, Write
---

## Capture a SigMF fixture

Fixtures are the test suite's ground truth (`docs/10`). Prefer real captures over synthetic where the use case allows.

### Before you open a device
- **Check it's free.** `hackrf_info` (HackRF) / `rtl_test -t` (NooElec/RTL-SDR). Only one process can hold a radio. If the demo on `:8899` holds it, coordinate handoff — don't fight for the device.
- **Receive only. Never transmit.**

### Capturing
- Address the RTL-SDR **by serial** (7673444264), never index.
- Choose centre/rate/gains for the signal; model the device's real limits (HackRF ≤ 20 Msps 8-bit no preselector; RTL ≤ ~2.4 Msps 8-bit, ~25 MHz–1.75 GHz).
- Small fixtures go in `fixtures/` (Git LFS) or the external store; reference them from the acceptance test **by use-case ID**.

### Record every setting in the SigMF metadata
Centre frequency, sample rate, LNA/VGA/amp gains, bias-tee, antenna, device serial, and the capture time. This is the **provenance** the data model records — a detection is only trustworthy with its gain state / overload flags / spur mask.

### Ground truth
Carry a **hidden** truth list of the interesting emissions in the fixture (blind tests find them without lookup-and-tune). If a target isn't receivable (antenna/location — e.g. BART at 851 MHz wasn't reachable at 1177 Market St with the stock antenna), that's a **hardware finding for the user**, not a software bug — report it and offer the alternative (a different reachable first instance, or a physical RF change).

### Release
Close the device when done, and tell the coordinator the radio is free.
