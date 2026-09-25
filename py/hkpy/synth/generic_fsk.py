"""The generic FSK/OOK sweep generator for ADR-0015 section 7 (T-863 = MAUTO M-12).

ADR-0015 section 7's evaluation table has one row no protocol owns::

    Generic FSK/OOK sweep (synthetic: 300 Bd-50 kBd, random 16-32-bit sync, RevEng-catalogue
    CRC-8/16, SNR 6/10/20 dB, CFO +/- 0.2 x bandwidth)   templates off: solved >= 80 % at 20 dB,
    >= 60 % at 10 dB; >= 50 % at least `framed` at 6 dB

and a "partial quality for unknowns" clause over the same population with the check made
unknowable to a catalogue (a random polynomial) or absent. This module draws one scene per seed
from that population. Everything a blind engine must *find* - modulation, symbol rate, deviation,
sync word, check parameters, CFO - is drawn here and written only into ``hackriff:truth``; the
engine sees the IQ through the mock SDR and nothing else.

One scene is one emitter sending ``n_bursts`` frames inside a window no longer than the analyze
job's 2 s IQ cap (``hk_pipeline::synth::jobs::MAX_WINDOW_NS``), so a job asked about the emitter
reads the whole scene: the first 60 % is the search window, the rest the hold-out.

Frame, air order MSB first, NRZ (bit 1 = +deviation for FSK, carrier on for OOK)::

    preamble (alternating, ``preamble_bits``) | sync (16-32 random bits) | payload | check

The payload differs per frame (a counter byte plus random bytes), so every frame is a distinct
piece of evidence for a check (ADR-0022 section 4.2: repeats are one piece of evidence).

``deepest_achievable`` (the verdict a perfect engine could reach, ADR-0015 section 3.4's ladder)
is stated **a priori** from the scene's construction, never from any engine's output:

- ``solved`` needs a check and at least :data:`MIN_HOLDOUT_FRAMES` whole frames inside the
  hold-out part of the window (ADR-0015 section 3.1's solve rule reads hold-out only). A random
  polynomial is recoverable in principle from frame differences (``hk_estimate::assist::codes``
  step 3), so it can still reach ``solved``; the partial-quality clause asks whether the verdict
  matches this field, not whether the check was catalogued.
- ``framed`` when there is no check, or too few hold-out frames to vouch for one.

No field here is unverified physics: FSK bandwidth is Carson's rule with f_m = Rs / 2 (as
``fsk_burst_train``), OOK's is the NRZ main lobe (2 x Rs), and SNR is in that bandwidth.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import fsk
from hkpy.synth.scenarios import DEFAULT_START_UTC, Ctx, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

#: ADR-0015 section 7's symbol-rate span.
RATE_MIN_BD = 300.0
RATE_MAX_BD = 50_000.0
#: ADR-0015 section 7's sync-word lengths.
SYNC_BITS_MIN = 16
SYNC_BITS_MAX = 32
#: ADR-0015 section 7's CFO bound, x the emission's bandwidth.
CFO_FRAC_MAX = 0.2
#: The check widths the section 7 sweep draws from the RevEng catalogue.
SWEEP_CHECK_WIDTHS = (8, 16)
#: Whole frames the hold-out must hold for a check to be able to vouch (ADR-0015 section 3.1:
#: "64 bits / 3 frames" solve rule).
MIN_HOLDOUT_FRAMES = 3
#: The search share of the window (ADR-0015 section 3.1, ``SEARCH_FRACTION`` in hk-synth).
SEARCH_FRACTION = 0.6
#: The families this generator draws.
MODULATIONS = ("2fsk", "ook")
#: The check kinds: RevEng catalogue CRC-8/16 (the sweep), an off-catalogue random polynomial or
#: no check at all (the partial-quality clause). Not "none": the scenario CLI reads that as null.
CHECKS = ("catalogue", "random-poly", "absent")

GENERIC_FSK_DEFAULTS: dict[str, Any] = {
    # Wide enough for the widest draw: 50 kBd at h = 2 is 150 kHz, +/-0.2 x bw of CFO, off DC.
    "sample_rate": 500e3,
    "center_hz": 868.3e6,
    "duration_s": 2.0,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    "snr_db": 20.0,
    # "draw" picks per seed from MODULATIONS; or name one.
    "modulation": "draw",
    # <= 0 -> drawn log-uniform in [RATE_MIN_BD, RATE_MAX_BD].
    "symbol_rate_bd": 0.0,
    # FSK modulation index h = 2 * deviation / Rs; <= 0 -> drawn uniform in [0.8, 2.0].
    "mod_index": 0.0,
    # None -> drawn uniform in [-CFO_FRAC_MAX, +CFO_FRAC_MAX] x bandwidth.
    "cfo_frac": None,
    # Where the nominal channel sits in the capture, x sample_rate (kept off DC).
    "channel_offset_frac": 0.12,
    # One of CHECKS.
    "check": "catalogue",
    # 0 -> drawn from SWEEP_CHECK_WIDTHS.
    "check_width": 0,
    # 0 -> drawn in [SYNC_BITS_MIN, SYNC_BITS_MAX].
    "sync_bits": 0,
    "preamble_bits": 16,
    # Frames per scene; fewer are sent when a slow rate cannot fit them (truth says how many).
    "n_bursts": 10,
    # Most payload bytes per frame (drawn in [2, this], then shrunk to fit a slow rate).
    "payload_bytes_max": 8,
    # Idle between frames, x one frame's duration.
    "gap_frac": 0.35,
}


def _draw_rate(rng: np.random.Generator, p: dict[str, Any]) -> float:
    r = float(p["symbol_rate_bd"])
    if r > 0:
        return r
    return float(math.exp(rng.uniform(math.log(RATE_MIN_BD), math.log(RATE_MAX_BD))))


def autocorrelation_sidelobe(bits: np.ndarray) -> int:
    """Peak aperiodic autocorrelation sidelobe of a +/-1 word (0 for a 1-bit word)."""
    s = 2 * np.asarray(bits, dtype=np.int64) - 1
    return int(max((abs(int(np.dot(s[k:], s[:-k]))) for k in range(1, len(s))), default=0))


def draw_sync(rng: np.random.Generator, n_bits: int) -> np.ndarray:
    """A random ``n_bits`` sync word a framer can find: balanced within n/4, a low aperiodic
    autocorrelation sidelobe (<= n/3), and not a run of the alternating preamble. Rejection
    sampling; deterministic in ``rng``."""
    if not SYNC_BITS_MIN <= n_bits <= SYNC_BITS_MAX:
        raise ValueError(f"sync_bits must be in [{SYNC_BITS_MIN}, {SYNC_BITS_MAX}], got {n_bits}")
    for _ in range(10_000):
        w = rng.integers(0, 2, n_bits).astype(np.uint8)
        ones = int(w.sum())
        if abs(2 * ones - n_bits) > n_bits // 4:
            continue
        if autocorrelation_sidelobe(w) > n_bits // 3:
            continue
        head = w[:8]
        if np.all(head[1:] != head[:-1]):
            continue  # starts like the 1010 preamble: the framer's boundary would be ambiguous
        return w
    raise RuntimeError("no admissible sync word found")  # pragma: no cover - never at n >= 16


def _random_poly(rng: np.random.Generator, width: int) -> dict[str, Any]:
    """RevEng parameters with a random polynomial that is in no catalogue entry of ``width``."""
    mask = (1 << width) - 1
    for _ in range(10_000):
        poly = int(rng.integers(1, mask + 1)) | 1  # a constant term, as every CRC generator has
        params = {"width": width, "poly": poly, "init": int(rng.integers(0, mask + 1)),
                  "refin": False, "refout": False, "xorout": 0}
        catalogued = any(v[0] == width and v[1] == poly for v in fsk.CRC_CATALOGUE.values())
        if not catalogued:
            return params
    raise RuntimeError("no off-catalogue polynomial found")  # pragma: no cover


def _catalogue_entry(rng: np.random.Generator, width: int) -> tuple[str, dict[str, Any]]:
    names = sorted(n for n, v in fsk.CRC_CATALOGUE.items() if v[0] == width)
    name = names[int(rng.integers(0, len(names)))]
    w, poly, init, refin, refout, xorout = fsk.CRC_CATALOGUE[name]
    return name, {"width": w, "poly": poly, "init": init, "refin": refin, "refout": refout,
                  "xorout": xorout}


def ook(bits: np.ndarray, sample_rate: float, symbol_rate: float) -> np.ndarray:
    """NRZ on-off keying at unit on-power: bit 1 -> carrier on, bit 0 -> off."""
    n = int(math.ceil(len(bits) * sample_rate / symbol_rate))
    sym = np.minimum((np.arange(n) * symbol_rate / sample_rate).astype(np.int64), len(bits) - 1)
    return np.asarray(bits, dtype=np.float64)[sym].astype(np.complex128)


def draw(seed_rng: np.random.Generator, p: dict[str, Any]) -> dict[str, Any]:
    """Every hidden parameter of one scene, drawn from ``p``'s population. Pure in the rng."""
    modulation = str(p["modulation"])
    if modulation == "draw":
        modulation = MODULATIONS[int(seed_rng.integers(0, len(MODULATIONS)))]
    if modulation not in MODULATIONS:
        raise ValueError(f"modulation must be 'draw' or one of {MODULATIONS}, got {modulation!r}")
    check = str(p["check"])
    if check not in CHECKS:
        raise ValueError(f"check must be one of {CHECKS}, got {check!r}")
    rate = _draw_rate(seed_rng, p)
    h = float(p["mod_index"]) if float(p["mod_index"]) > 0 else float(seed_rng.uniform(0.8, 2.0))
    deviation = h * rate / 2 if modulation == "2fsk" else 0.0
    bandwidth = 2 * deviation + rate if modulation == "2fsk" else 2 * rate
    cfo_frac = p["cfo_frac"]
    cfo_frac = float(seed_rng.uniform(-CFO_FRAC_MAX, CFO_FRAC_MAX)) if cfo_frac is None \
        else float(cfo_frac)
    sync_bits = int(p["sync_bits"]) or int(seed_rng.integers(SYNC_BITS_MIN, SYNC_BITS_MAX + 1))
    width = int(p["check_width"]) or int(SWEEP_CHECK_WIDTHS[int(seed_rng.integers(0, 2))])
    if check != "absent" and width not in (8, 16, 24, 32):
        raise ValueError(f"check_width must be 8/16/24/32, got {width}")
    return {
        "modulation": modulation,
        "symbol_rate_bd": rate,
        "mod_index": h if modulation == "2fsk" else None,
        "deviation_hz": deviation if modulation == "2fsk" else None,
        "bandwidth_hz": bandwidth,
        "cfo_frac": cfo_frac,
        "cfo_hz": cfo_frac * bandwidth,
        "sync_bits": sync_bits,
        "check": check,
        "check_width": width if check != "absent" else 0,
    }


def frame_plan(rate: float, duration_s: float, n_bursts: int, gap_frac: float,
               fixed_bits: int, payload_max: int) -> tuple[int, int, float]:
    """``(payload_bytes, n_frames, period_s)``: the largest payload (2..payload_max) that still
    fits ``n_bursts`` frames with their gaps into the window, or - at a rate too slow for that -
    the 2-byte payload and as many frames as fit (at least 1)."""
    usable = duration_s * 0.95
    for pb in range(payload_max, 1, -1):
        frame_s = (fixed_bits + 8 * pb) / rate
        if n_bursts * frame_s * (1 + gap_frac) <= usable:
            return pb, n_bursts, usable / n_bursts
    frame_s = (fixed_bits + 16) / rate
    n = max(1, int(usable // (frame_s * (1 + gap_frac))))
    return 2, n, usable / n


def generic_fsk_sweep(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    """One draw from ADR-0015 section 7's generic FSK/OOK population (module docs)."""
    p = ctx.params
    fs = float(p["sample_rate"])
    n = int(round(float(p["duration_s"]) * fs))
    scene = ctx.scene("generic_fsk_sweep", fs, n,
                      "hkpy.synth generic_fsk_sweep: ADR-0015 section 7 generic FSK/OOK draw")
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    d = draw(scene.rng("draw"), p)
    rate, bw = d["symbol_rate_bd"], d["bandwidth_hz"]
    nominal = float(p["channel_offset_frac"]) * fs
    off = nominal + d["cfo_hz"]
    if abs(off) + bw / 2 > fs / 2:
        raise ValueError(f"emission ({bw:.0f} Hz at {off:+.0f} Hz) leaves the {fs:.0f} Hz window")

    frames_rng = scene.rng("frames")
    sync = draw_sync(scene.rng("sync"), d["sync_bits"])
    catalogue_name: str | None = None
    crc_params: dict[str, Any] | None = None
    if d["check"] == "catalogue":
        catalogue_name, crc_params = _catalogue_entry(scene.rng("check"), d["check_width"])
    elif d["check"] == "random-poly":
        crc_params = _random_poly(scene.rng("check"), d["check_width"])
    preamble = np.array([(i + 1) % 2 for i in range(int(p["preamble_bits"]))], dtype=np.uint8)
    fixed_bits = len(preamble) + len(sync) + d["check_width"]
    payload_bytes, n_frames, period_s = frame_plan(
        rate, float(p["duration_s"]), int(p["n_bursts"]), float(p["gap_frac"]), fixed_bits,
        int(p["payload_bytes_max"]))
    payload_bytes = min(payload_bytes, int(frames_rng.integers(2, payload_bytes + 1)))
    frame_bits = fixed_bits + 8 * payload_bytes
    frame_s = frame_bits / rate

    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    holdout_from_s = SEARCH_FRACTION * float(p["duration_s"])
    frames: list[dict[str, Any]] = []
    for k in range(n_frames):
        payload = bytes([k & 0xFF]) + bytes(frames_rng.integers(0, 256, payload_bytes - 1)
                                            .astype(np.uint8).tolist())
        check_value: int | None = None
        parts = [preamble, sync, fsk.bytes_to_bits(payload)]
        if crc_params is not None:
            check_value = fsk.crc_generic(payload, **crc_params)
            cb = d["check_width"] // 8
            parts.append(fsk.bytes_to_bits(check_value.to_bytes(cb, "big")))
        bits = np.concatenate(parts)
        if d["modulation"] == "2fsk":
            base = fsk.cpfsk(bits, fs, rate, d["deviation_hz"],
                             phase0=float(frames_rng.uniform(0, 2 * math.pi)))
        else:
            base = ook(bits, fs, rate) * np.exp(1j * float(frames_rng.uniform(0, 2 * math.pi)))
        t0 = 0.02 * float(p["duration_s"]) + k * period_s
        start = int(round(t0 * fs))
        if start + len(base) > n:
            break
        tt = scene.time(start, len(base))
        scene.add_samples(start, amp * base * np.exp(2j * math.pi * off * tt))
        f = cap.center_hz + off
        truth = scene.emission_truth(
            cap, off, bw, power, kind="generic-fsk-frame", modulation=d["modulation"],
            levels=2, symbol_rate_bd=rate, deviation_hz=d["deviation_hz"],
            nominal_center_hz=cap.center_hz + nominal, cfo_hz=d["cfo_hz"], burst_index=k,
            bit_order="msb-first", line_code="nrz",
            frame={"n_bits": int(len(bits)), "bits_hex": fsk.bits_to_hex(bits),
                   "sync_hex": fsk.bits_to_hex(sync), "sync_bits": int(len(sync)),
                   "payload_hex": payload.hex(),
                   "check_hex": None if check_value is None else f"{check_value:x}"})
        scene.annotate(start, len(base), f - bw / 2, f + bw / 2, "generic-fsk-frame", truth)
        frames.append({"t_start_s": start / fs, "t_end_s": (start + len(base)) / fs})

    holdout_frames = sum(1 for fr in frames if fr["t_start_s"] >= holdout_from_s)
    if crc_params is not None and holdout_frames >= MIN_HOLDOUT_FRAMES:
        achievable, why = "solved", (
            f"a check and {holdout_frames} whole frame(s) in the hold-out "
            f"(>= {MIN_HOLDOUT_FRAMES})")
    elif crc_params is None:
        achievable, why = "framed", "no check: a sync word frames the bits and nothing verifies them"
    else:
        achievable, why = "framed", (
            f"only {holdout_frames} whole frame(s) in the hold-out (< {MIN_HOLDOUT_FRAMES}), so "
            f"no check can vouch on hold-out")
    check_truth: dict[str, Any] | None = None
    if crc_params is not None:
        hw = max(2, d["check_width"] // 4)
        check_truth = {
            "kind": "crc", "width": d["check_width"],
            "poly": f"0x{crc_params['poly']:0{hw}x}", "init": f"0x{crc_params['init']:0{hw}x}",
            "refin": crc_params["refin"], "refout": crc_params["refout"],
            "xorout": f"0x{crc_params['xorout']:0{hw}x}",
            "in_reveng_catalogue": catalogue_name is not None, "catalogue_name": catalogue_name,
            "covers": "payload bytes", "bit_order": "msb-first",
        }
    scene.scenario_truth["generic_fsk"] = {
        "population": "ADR-0015 section 7 generic FSK/OOK sweep",
        "modulation": d["modulation"],
        "rf_center_hz": cap.center_hz + off,
        "nominal_center_hz": cap.center_hz + nominal,
        "bandwidth_hz": bw,
        "snr_db": float(p["snr_db"]),
        "symbol_rate_bd": rate,
        "mod_index": d["mod_index"],
        "deviation_hz": d["deviation_hz"],
        "cfo_hz": d["cfo_hz"],
        "cfo_frac": d["cfo_frac"],
        "sync_hex": fsk.bits_to_hex(sync),
        "sync_bits": int(len(sync)),
        "sync_bitstring": "".join(str(int(b)) for b in sync),
        "preamble_bits": int(len(preamble)),
        "payload_bytes": payload_bytes,
        "frame_bits": frame_bits,
        "frame_s": frame_s,
        "n_frames": len(frames),
        "holdout_frames": holdout_frames,
        "check_kind": d["check"],
        "check": check_truth,
        "deepest_achievable": achievable,
        "deepest_achievable_reason": why,
        "frames": frames,
    }
    return [scene], {}
