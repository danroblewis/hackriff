"""N5 — the mismatched-hypothesis negative population (T-626).

**The null the search actually faces.** ADR-0021 §8.4 names four negative populations: thermal
noise, energy without symbols, out-of-catalogue structure, and a real quiet capture. T-547
measured what the search's *actual* null looks like and it is none of those. On pure noise the
6-bit calibrated tables over-claimed **≤ 0.34 bits**; on a **mismatched-parameter** null — right
block, wrong symbol rate — they over-claimed **up to 1.75 bits**, five times worse, and that is
before an adjacent real emitter is in the window (docs/22 §4.3). A negative suite built only from
the four listed populations measures the engine against the friendliest null available and would
report a comfortable margin while the operational null was five times fatter.

**It is a FIXTURE population, not engine configuration**, because the blind rule forbids steering
the search. Two shapes, both produced here:

- ``off_grid`` — a real, framed, CRC-valid 2-FSK emitter whose **true symbol rate and modulation
  index lie outside the proposal grid** (:data:`PROPOSAL_SYMBOL_RATES_BD`,
  :data:`PROPOSAL_MOD_INDICES`), so the engine necessarily evaluates wrong-parameter hypotheses in
  quantity. Nothing on the grid is right; something on the grid will always score best.
- ``adjacent_leakage`` — the same emitter with a **strong hard-keyed neighbour** outside the
  analysed box whose skirts land inside it. The neighbour is never in the box, so any parameter
  the engine binds *from* it is a mismatch by construction.

**What must NOT happen** is ``solved`` with wrong parameters. ``unknown`` with ``reason: tied``,
or a correct partial result, are both fine. :func:`classify_outcome` is the piece the assert
harness needs: it turns a result plus the hidden truth into ``correct`` / ``partial`` /
``mismatch`` / ``abstain``, so a **mismatch is distinguishable from a miss** without the system
ever seeing a truth value — the blind ground-truth rule (fixtures carry a hidden truth list, the
test runs blind detection and checks the finding, never looks a value up and tunes to it).
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np
from scipy import signal

from hkpy.synth import fsk
from hkpy.synth.scene import Scene, complex_noise, db, undb

#: The symbol rates a blind search proposes first: the standard telemetry/paging ladder. Declared
#: here because the corpus must state the grid its scenes fall outside of; the engine's own grid
#: (ADR-0015 §3.2's proposal operators) does not exist yet, and a fixture that could only be
#: checked against a future implementation would be untestable today.
PROPOSAL_SYMBOL_RATES_BD: tuple[float, ...] = (
    300.0, 600.0, 1200.0, 2400.0, 4800.0, 9600.0, 19200.0, 38400.0,
)

#: The modulation indices a blind FSK search proposes (h = 2·deviation / symbol rate).
PROPOSAL_MOD_INDICES: tuple[float, ...] = (0.5, 1.0, 2.0)

#: A parameter this far (fractionally) from every grid point cannot be reached by a grid hypothesis.
#: Chosen well above a symbol-clock acquisition tolerance (~1–2 %), so "off grid" is not a rounding
#: argument.
OFF_GRID_MIN_DISTANCE = 0.10

#: Fractional symbol-rate agreement within which a bound parameter counts as the true one.
SYMBOL_RATE_TOLERANCE = 0.02

MISMATCH_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    "center_hz": 433.92e6,
    "population": "off_grid",  # off_grid | adjacent_leakage
    "channel_offset_hz": -40e3,
    "cfo_hz": 0.0,
    # Off the grid on BOTH axes: 1873 Bd is 22 % from 2400 Bd, h = 1.281 is 28 % from 1.0.
    "symbol_rate_bd": 1873.0,
    "deviation_hz": 1200.0,
    "snr_db": 20.0,
    "noise_dbfs": -40.0,
    "duration_s": 0.6,
    "first_burst_s": 0.03,
    "period_s": 0.12,
    "jitter_s": 0.0,
    "preamble_bits": 32,
    "sync_hex": "2dd4",
    "sensor_id": 0x5A3C,
    # The adjacent emitter: a hard-keyed (unshaped) strong carrier whose sinc skirts reach the box.
    "adjacent_offset_hz": -10e3,
    "adjacent_excess_db": 34.0,
    "adjacent_keying_bd": 2000.0,
    "calibration_k_db": -70.0,
    "start_utc": "2026-09-13T12:00:00Z",
}

POPULATIONS = ("off_grid", "adjacent_leakage")


def grid_distance(value: float, grid: tuple[float, ...]) -> tuple[float, float]:
    """``(nearest grid point, fractional distance to it)``, the distance measured against the grid
    point — which is the tolerance an engine testing that hypothesis would apply."""
    nearest = min(grid, key=lambda g: abs(value - g) / g)
    return float(nearest), abs(value - nearest) / nearest


def classify_outcome(result: dict[str, Any] | None, truth: dict[str, Any]) -> dict[str, Any]:
    """Turns an analyze result into an N5 outcome, using the hidden truth only to *judge*.

    ``result`` is the rank-1 ``PipelineResult`` shape the harness sees: ``verdict``,
    ``resolution`` (``kind`` / ``reason``) and the concrete parameters the recipe bound.
    ``truth`` is this scene's ``negative_population`` truth block.

    The four outcomes, and why the distinction is the point of this population:

    - ``correct`` — bound the true parameters. A real result on a real signal.
    - ``partial`` — a verdict below ``framed``, or bound nothing, while claiming no solution.
    - ``abstain`` — ``unknown`` (any reason). **The wanted answer**, and NOT a miss: there is a
      real emitter present, so an engine that declines to name its parameters has behaved.
    - ``mismatch`` — a label (``verdict ≥ framed``) with parameters that are **not** the true
      ones. This is the over-claim N5 counts, and ``grid_snapped`` records the aggravating case:
      it bound a *proposal-grid* value, i.e. it answered the hypothesis it had rather than the
      signal it saw.

    A miss — nothing found at all — is ``abstain`` here on purpose: N5 measures false labels, not
    recall. The recall claim is the P rows' job, and conflating them is how a suite reports a
    comfortable margin by being bad at finding things.
    """
    verdict_rank = {"energy": 0, "demodulated": 1, "clocked": 2, "framed": 3, "checked": 4,
                    "solved": 5}
    if not result:
        return {"outcome": "abstain", "reason": "no-result", "grid_snapped": False}
    kind = (result.get("resolution") or {}).get("kind")
    rank = verdict_rank.get(result.get("verdict", "energy"), 0)
    bound = result.get("symbol_rate_bd")
    if kind == "unknown":
        return {"outcome": "abstain", "reason": (result.get("resolution") or {}).get("reason"),
                "grid_snapped": False}
    if rank < verdict_rank["framed"] or bound is None:
        return {"outcome": "partial", "reason": "below-framed", "grid_snapped": False}

    true_rate = float(truth["off_grid"]["symbol_rate_bd"])
    error = abs(float(bound) - true_rate) / true_rate
    if error <= SYMBOL_RATE_TOLERANCE:
        return {"outcome": "correct", "symbol_rate_error_frac": error, "grid_snapped": False}
    _, grid_error = grid_distance(float(bound), PROPOSAL_SYMBOL_RATES_BD)
    return {
        "outcome": "mismatch",
        "symbol_rate_error_frac": error,
        "grid_snapped": grid_error <= SYMBOL_RATE_TOLERANCE,
    }


def _target_bursts(scene: Scene, p: dict[str, Any], cap: Any, amp: float, off: float,
                   bw: float, power: float) -> int:
    """The real, framed, CRC-valid emitter — at parameters no grid hypothesis can reach."""
    fs = scene.sample_rate
    rate, dev = float(p["symbol_rate_bd"]), float(p["deviation_hz"])
    sync = bytes.fromhex(p["sync_hex"])
    preamble = np.array([(i + 1) % 2 for i in range(int(p["preamble_bits"]))], dtype=np.uint8)
    sensor_id = int(p["sensor_id"]) & 0xFFFF
    sched = scene.rng("schedule")
    payload_rng = scene.rng("payload")
    k = 0
    while True:
        t = float(p["first_burst_s"]) + k * float(p["period_s"])
        jitter = float(p["jitter_s"])
        t += float(sched.uniform(-jitter, jitter)) if jitter > 0 else 0.0
        payload = ((sensor_id << 32) | ((k & 0xFF) << 24)
                   | (int(payload_rng.integers(0, 0xFFF)) << 12)
                   | (int(payload_rng.integers(0, 0xFF)) << 4) | 0b0001).to_bytes(6, "big")
        crc = fsk.crc16_ccitt_false(payload)
        bits = np.concatenate([preamble, fsk.bytes_to_bits(sync), fsk.bytes_to_bits(payload),
                               fsk.bytes_to_bits(crc.to_bytes(2, "big"))])
        iq = fsk.cpfsk(bits, fs, rate, dev, phase0=float(sched.uniform(0, 2 * math.pi)))
        start = max(0, int(round(t * fs)))
        if start + len(iq) > scene.n_samples:
            return k
        tt = scene.time(start, len(iq))
        scene.add_samples(start, amp * iq * np.exp(2j * math.pi * off * tt))
        truth = scene.emission_truth(
            cap, off, bw, power, kind="fsk-burst", modulation="2fsk", levels=2,
            symbol_rate_bd=rate, deviation_hz=dev, mod_index=2 * dev / rate,
            in_analysed_box=True, burst_index=k, bit_order="msb-first",
            frame={"n_bits": int(len(bits)), "bits_hex": fsk.bits_to_hex(bits),
                   "preamble_bits": int(len(preamble)), "sync_hex": sync.hex(),
                   "payload_hex": payload.hex(), "crc_hex": f"{crc:04x}"},
            crc={**fsk.CRC16_SPEC, "value": f"0x{crc:04X}", "valid": True},
            identity={"type": "sensor_id", "value": f"{sensor_id:04x}"},
        )
        f = cap.center_hz + off
        scene.annotate(start, len(iq), f - bw / 2, f + bw / 2, "fsk-burst", truth)
        k += 1


def _adjacent_emitter(scene: Scene, p: dict[str, Any], cap: Any, power: float,
                      box: tuple[float, float]) -> dict[str, Any]:
    """A strong neighbour OUTSIDE the analysed box whose hard-keying skirts land inside it.

    Hard (unshaped) on/off keying is the mechanism, not a fudge: a rectangular envelope has sinc
    skirts falling as 1/f², so a neighbour 34 dB up raises the level inside a box it never
    occupies. Nothing about it is in the box, so every parameter an engine binds from it is wrong
    by construction — which is the second half of N5.
    """
    fs = scene.sample_rate
    off = float(p["adjacent_offset_hz"])
    keying = float(p["adjacent_keying_bd"])
    amp = math.sqrt(undb(power))
    n = scene.n_samples
    tt = scene.time(0, n)
    keyed = ((np.arange(n) * keying / fs).astype(np.int64) % 2).astype(np.float64)
    samples = amp * keyed * np.exp(2j * math.pi * off * tt)
    scene.add_samples(0, samples)

    # The leak, measured on the samples this function just wrote — never asserted, and never a
    # model of what the skirts "should" be.
    nperseg = min(1 << 13, len(samples))
    f, pxx = signal.welch(samples, fs=fs, nperseg=nperseg, return_onesided=False, detrend=False)
    f, pxx = np.fft.fftshift(f), np.fft.fftshift(pxx)
    in_box = (f + cap.center_hz >= box[0]) & (f + cap.center_hz <= box[1])
    psd = float(np.mean(pxx[in_box])) if in_box.any() else 0.0
    leak_psd_dbfs_per_hz = db(psd) if psd > 0 else -300.0
    leak_dbfs = leak_psd_dbfs_per_hz + db(box[1] - box[0])
    floor_dbfs = float(cap.floor_dbfs_per_hz) + db(box[1] - box[0])

    bw = 2 * keying
    f = cap.center_hz + off
    truth = scene.emission_truth(
        cap, off, bw, power, kind="adjacent-interferer", modulation="ook",
        keying_bd=keying, in_analysed_box=False, leaks_into_box=True,
        shaping="none (rectangular envelope: sinc skirts, 1/f^2)",
        leak_in_box_dbfs=leak_dbfs, leak_in_box_dbfs_per_hz=leak_psd_dbfs_per_hz,
        leak_margin_over_floor_db=leak_dbfs - floor_dbfs,
        note="outside the box in frequency; any parameter bound from it is a mismatch",
    )
    scene.annotate(0, n, f - bw / 2, f + bw / 2, "adjacent-interferer", truth)
    return {
        "offset_hz": off,
        "rf_center_hz": f,
        "power_dbfs": power,
        "keying_bd": keying,
        "bandwidth_hz": bw,
        "leak_in_box_dbfs": leak_dbfs,
        "leak_in_box_dbfs_per_hz": leak_psd_dbfs_per_hz,
        "box_floor_dbfs": floor_dbfs,
        "leak_margin_over_floor_db": leak_dbfs - floor_dbfs,
    }


def mismatched_hypothesis(ctx: Any) -> tuple[list[Scene], dict[str, Any]]:
    """The N5 population (docs/22 §4.3). ``population`` selects the shape."""
    p = ctx.params
    population = str(p["population"])
    if population not in POPULATIONS:
        raise ValueError(f"population must be one of {POPULATIONS}, got {population!r}")
    fs = float(p["sample_rate"])
    n = int(round(float(p["duration_s"]) * fs))
    scene = ctx.scene(
        f"mismatched_hypothesis_{population}", fs, n,
        "hkpy.synth mismatched_hypothesis: a real framed emitter whose parameters lie outside the "
        "proposal grid (N5)",
    )
    cap = scene.add_capture(0, n, float(p["center_hz"]), p["start_utc"],
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    rate, dev = float(p["symbol_rate_bd"]), float(p["deviation_hz"])
    mod_index = 2 * dev / rate
    bw = 2 * dev + rate
    off = float(p["channel_offset_hz"]) + float(p["cfo_hz"])
    if abs(off) + bw / 2 > fs / 2:
        raise ValueError("the emitter does not fit inside the sample rate")
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    n_bursts = _target_bursts(scene, p, cap, math.sqrt(undb(power)), off, bw, power)

    box = (cap.center_hz + off - bw / 2, cap.center_hz + off + bw / 2)
    leakage = None
    if population == "adjacent_leakage":
        adj_off = float(p["adjacent_offset_hz"])
        if abs(adj_off) + float(p["adjacent_keying_bd"]) > fs / 2:
            raise ValueError("the adjacent emitter does not fit inside the sample rate")
        if box[0] <= cap.center_hz + adj_off <= box[1]:
            raise ValueError("the adjacent emitter must sit OUTSIDE the analysed box")
        leakage = _adjacent_emitter(
            scene, p, cap, power + float(p["adjacent_excess_db"]), box
        )

    near_rate, rate_distance = grid_distance(rate, PROPOSAL_SYMBOL_RATES_BD)
    near_h, h_distance = grid_distance(mod_index, PROPOSAL_MOD_INDICES)
    scene.scenario_truth["negative_population"] = {
        "id": "N5",
        "name": "mismatched hypothesis",
        "spec": "docs/22 §4.3; ADR-0021 §8.4 populations N1–N4 plus this one",
        "shape": population,
        "why": (
            "T-547: on pure noise the 6-bit tables over-claimed <= 0.34 bits; on a "
            "mismatched-parameter null (right block, wrong symbol rate) they over-claimed up to "
            "1.75 bits. This is the null the search spends its time on."
        ),
        "must_never_return": "solved (or any verdict >= framed) with parameters that are not the "
                             "true ones",
        "acceptable": ["unknown/tied", "a correct partial result"],
        "counted_in": ["false_labels (N1 u N2 u N4 u N5)",
                       "ADR-0022 A2 max analytic_holdout_bits"],
        "proposal_grid": {
            "symbol_rate_bd": list(PROPOSAL_SYMBOL_RATES_BD),
            "mod_index": list(PROPOSAL_MOD_INDICES),
            "off_grid_min_distance": OFF_GRID_MIN_DISTANCE,
        },
        "off_grid": {
            "symbol_rate_bd": rate,
            "nearest_grid_symbol_rate_bd": near_rate,
            "symbol_rate_grid_distance": rate_distance,
            "mod_index": mod_index,
            "nearest_grid_mod_index": near_h,
            "mod_index_grid_distance": h_distance,
            "deviation_hz": dev,
        },
        "analysed_box": {
            "f_lo_hz": box[0],
            "f_hi_hz": box[1],
            "bandwidth_hz": bw,
            "rf_center_hz": cap.center_hz + off,
        },
        "adjacent_leakage": leakage,
        "mismatch_vs_miss": {
            "rule": "hkpy.synth.mismatch.classify_outcome",
            "symbol_rate_tolerance_frac": SYMBOL_RATE_TOLERANCE,
            "correct": "verdict >= framed and |bound - true| / true <= tolerance",
            "mismatch": "verdict >= framed and the bound rate is not the true one; "
                        "`grid_snapped` when it is a proposal-grid value instead",
            "abstain": "resolution.kind == unknown (any reason), or nothing returned - the "
                       "WANTED answer here, and deliberately not scored as a miss: N5 measures "
                       "false labels, not recall",
        },
        "n_bursts": n_bursts,
    }
    scene.scenario_truth["emitter"] = {
        "identity": {"type": "sensor_id", "value": f"{int(p['sensor_id']) & 0xFFFF:04x}"},
        "expected_known_status": "unknown",
        "modulation": "2fsk",
        "rf_center_hz": cap.center_hz + off,
        "bandwidth_hz": bw,
        "symbol_rate_bd": rate,
        "deviation_hz": dev,
        "mod_index": mod_index,
        "n_bursts": n_bursts,
        "crc": fsk.CRC16_SPEC,
    }
    return [scene], {}
