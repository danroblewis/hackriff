"""Two-path scene: one transmission, received twice (T-222, AWARE-053, C40 content half).

The question this scene poses is the one geometry cannot answer. Three narrowband 2-FSK burst
emitters sit in the same span, none of their bands overlapping, all of the same family, bandwidth,
symbol rate and deviation. Two of them are **the same transmission**: the second is a delayed,
attenuated copy of the first, burst for burst, bit for bit. The third is an independent station of
the same family, keying to its own schedule.

Nothing in the frequencies, the bandwidths or the modulations separates the pair from the decoy.
Only the **content** does: the copy's keying pattern is the direct path's, shifted by
``delay_s``; the decoy's is its own. A system that relates the pair must have measured that, and
one that relates the decoy has not.

**The schedule is aperiodic on purpose.** A strictly periodic burst train correlates with itself at
every multiple of its period, so a periodic scene could never pin *one* delay — and a rule that
claimed a path difference from it would be claiming more than it measured. Gaps are drawn from a
wide uniform range so the pattern is unique and the correlation peak is unambiguous.

Truth (hidden from the system, read only by the assertions) records which emitter is the direct
path, which is its echo, and the injected ``delay_s`` and ``attenuation_db``.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import fsk
from hkpy.synth.scenarios import DEFAULT_START_UTC, Ctx, _n, _noise_capture
from hkpy.synth.scene import Scene, db, undb

MULTIPATH_DEFAULTS: dict[str, Any] = {
    # 2 MSps, and the three channels half a megahertz apart. Not cosmetic: a burst detector
    # occasionally draws a wide one-frame box over a busy span, and at 100 kHz spacing one such box
    # bridges two channels and merges them into a single wide row before any content is compared.
    # Half a megahertz is wider than any box this detector has been seen to draw.
    "sample_rate": 2.0e6,
    "center_hz": 433.92e6,
    # No two occupied bands overlap; none sits near the tuning centre (where a DC artefact would
    # widen a box into its neighbour); and none is the mirror `2*f_LO - f` of another, so the image
    # rule has nothing to say about any pair either. Whatever relates two of them, it is not
    # geometry.
    "direct_offset_hz": -700e3,
    "echo_offset_hz": -200e3,
    "decoy_offset_hz": 600e3,
    # The injected path delay. 50 ms is ~15 000 km of extra path -- the long-path / multi-hop
    # regime, and the delay range a detection-record envelope can resolve honestly.
    "delay_s": 0.050,
    # A reflection loses energy. The copy must be weaker, and measurably so.
    "attenuation_db": 4.0,
    "symbol_rate_bd": 9600.0,
    "deviation_hz": 9600.0,
    # Gaussian-filtered FSK: an unfiltered CPFSK burst at this rate has skirts wide enough to
    # bridge two channels in one detector frame, which merges the scene into one wide row before
    # any content is compared. A real ISM sensor is filtered; so is this one.
    "bt": 0.5,
    "snr_db": 12.0,
    "noise_dbfs": -40.0,
    "duration_s": 8.0,
    "first_burst_s": 0.15,
    # Aperiodic: gaps uniform in [gap_min_s, gap_max_s]. See the module docstring.
    #
    # The duty cycle is high on purpose. At a few per cent the three channels are almost never on
    # air together, and a tracker reading three channels keying in turn has every reason to call
    # them ONE frequency-hopping emitter -- which is a sensible reading, and one that would merge
    # the scene into a single hop-set row before any content was compared. A hopper cannot
    # transmit on two channels at once, so overlapping keyings are what refute it, and at ~70 %
    # duty they are constant. The gaps stay RANDOM, so the keying pattern carries information: a
    # fixed cadence would repeat, and a repeating series cannot pin a lag at all (T-222 guards).
    "gap_min_s": 0.015,
    "gap_max_s": 0.075,
    # THE FALSE-POSITIVE MODE (review of 2026-09-22). With `pair_independent=1` the second channel
    # stops being a copy and becomes a SEPARATE emitter -- its own payloads, its own identity --
    # that merely keys `delay_s` after the first. With `cadence_s>0` both key on a FIXED period
    # instead of random gaps. Together they are the case that fooled the first cut of the rule: two
    # identical-model sensors on the same cadence, a sub-150 ms phase apart, whose envelopes
    # correlate a perfect 1.00 at that phase and whose only competitor -- the cadence itself --
    # repeats further away than the lag search ever looks. Nothing may relate them, and the weaker
    # one is a real emission that must stay visible.
    "pair_independent": 0,
    "cadence_s": 0.0,
    "preamble_bits": 256,
    "sync_hex": "2dd4",
    "sensor_id": 0x5A3C,
    "decoy_sensor_id": 0x7B21,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def _burst_bits(p: dict[str, Any], sensor_id: int, k: int, rng: np.random.Generator) -> np.ndarray:
    """One framed sensor burst: preamble, sync, payload, CRC-16."""
    preamble = np.array([(i + 1) % 2 for i in range(int(p["preamble_bits"]))], dtype=np.uint8)
    sync = bytes.fromhex(p["sync_hex"])
    temp_dc = int(rng.integers(100, 250))
    humidity = int(rng.integers(30, 70))
    payload_int = ((sensor_id & 0xFFFF) << 32) | ((k & 0xFF) << 24) | ((temp_dc & 0xFFF) << 12) \
        | ((humidity & 0xFF) << 4) | 0b0001
    payload = payload_int.to_bytes(6, "big")
    crc = fsk.crc16_ccitt_false(payload)
    return np.concatenate([preamble, fsk.bytes_to_bits(sync), fsk.bytes_to_bits(payload),
                           fsk.bytes_to_bits(crc.to_bytes(2, "big"))])


def _schedule(scene: Scene, p: dict[str, Any], name: str) -> list[float]:
    """Burst start times, seconds: aperiodic by default, fixed-cadence when ``cadence_s`` is set."""
    rng = scene.rng("schedule", name)
    cadence = float(p["cadence_s"])
    t = float(p["first_burst_s"])
    out: list[float] = []
    # Leave room for the delayed copy of the last burst.
    last = float(p["duration_s"]) - float(p["delay_s"]) - 0.1
    while t < last:
        out.append(t)
        t += cadence if cadence > 0 else float(
            rng.uniform(float(p["gap_min_s"]), float(p["gap_max_s"])))
    return out


def multipath_echo(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    scene = ctx.scene("multipath_echo", fs, _n(p),
                      "hkpy.synth multipath_echo: one 2-FSK transmission received twice "
                      "(delayed, attenuated copy) beside an independent station of the same family")
    _noise_capture(scene, p, p["center_hz"], p["calibration_k_db"])
    cap = scene.captures[0]
    rate, dev, bt = float(p["symbol_rate_bd"]), float(p["deviation_hz"]), float(p["bt"])
    bw = 2 * dev + rate  # Carson's rule with f_m = Rs/2
    power = float(p["snr_db"]) + cap.floor_dbfs_per_hz + db(bw)
    amp = math.sqrt(undb(power))
    echo_amp = amp * math.sqrt(undb(-float(p["attenuation_db"])))
    echo_power = power - float(p["attenuation_db"])
    delay_n = int(round(float(p["delay_s"]) * fs))

    for off in (float(p["direct_offset_hz"]), float(p["echo_offset_hz"]),
                float(p["decoy_offset_hz"])):
        if abs(off) + bw / 2 > fs / 2:
            raise ValueError(f"channel at {off} Hz does not fit inside the sample rate")

    # A transmitter ramps its PA rather than switching it: without this the burst starts as a step
    # discontinuity, which is a broadband click across the whole span in one frame -- and when three
    # onsets coincide the click is strong enough to be detected as a full-span box that joins every
    # channel into one row before any content is compared. The ramp is longer than one detector
    # frame (16 symbols ~ 1.7 ms at 9600 Bd), so no single frame ever sees a step.
    ramp_n = max(1, int(round(16.0 * fs / rate)))

    def ramped(iq: np.ndarray) -> np.ndarray:
        if len(iq) < 2 * ramp_n:
            return iq
        env = np.ones(len(iq))
        r = 0.5 * (1.0 - np.cos(np.pi * np.arange(ramp_n) / ramp_n))
        env[:ramp_n] = r
        env[-ramp_n:] = r[::-1]
        return iq * env

    def place(start: int, iq: np.ndarray, offset_hz: float, a: float) -> tuple[int, int] | None:
        if start < 0 or start + len(iq) > scene.n_samples:
            return None
        tt = scene.time(start, len(iq))
        scene.add_samples(start, a * ramped(iq) * np.exp(2j * math.pi * offset_hz * tt))
        return start, len(iq)

    # ---- the transmission, and its second path -------------------------------------------
    direct_off, echo_off = float(p["direct_offset_hz"]), float(p["echo_offset_hz"])
    payload_rng = scene.rng("payload", "direct")
    phase_rng = scene.rng("phase", "direct")
    independent = bool(int(p["pair_independent"]))
    pair_rng = scene.rng("payload", "pair")
    pair_phase_rng = scene.rng("phase", "pair")
    pair_id = (int(p["sensor_id"]) ^ 0x3C5A) & 0xFFFF
    n_bursts = 0
    for k, t in enumerate(_schedule(scene, p, "direct")):
        bits = _burst_bits(p, int(p["sensor_id"]), k, payload_rng)
        iq = fsk.cpfsk(bits, fs, rate, dev, bt=bt,
                       phase0=float(phase_rng.uniform(0, 2 * math.pi)))
        start = max(0, int(round(t * fs)))
        placed = place(start, iq, direct_off, amp)
        if placed is None:
            break
        # The second path: the SAME waveform, delayed and attenuated. Not a re-generation --
        # `iq` is reused, so the two copies are identical content by construction.
        #
        # Unless `pair_independent`, in which case the second channel is a DIFFERENT emitter with
        # its own payloads that merely keys `delay_s` later: same family, same bandwidth, same
        # burst length, same cadence, different content. Identical envelope, unrelated signal.
        second = iq
        if independent:
            second = fsk.cpfsk(_burst_bits(p, pair_id, k, pair_rng), fs, rate, dev, bt=bt,
                               phase0=float(pair_phase_rng.uniform(0, 2 * math.pi)))
        echo = place(start + delay_n, second, echo_off, echo_amp)
        if echo is None:
            break
        n_bursts += 1
        for (s, n), off, pw, kind in (
            (placed, direct_off, power, "fsk-burst"),
            (echo, echo_off, echo_power,
             "independent-station" if independent else "multipath-echo"),
        ):
            f = cap.center_hz + off
            truth = scene.emission_truth(
                cap, off, bw, pw, kind=kind, modulation="2fsk", levels=2,
                symbol_rate_bd=rate, deviation_hz=dev, mod_index=2 * dev / rate, bt=bt,
                burst_index=k, path="direct" if kind == "fsk-burst" else "echo",
                of_rf_center_hz=cap.center_hz + direct_off,
                delay_s=0.0 if kind == "fsk-burst" else float(p["delay_s"]),
                attenuation_db=0.0 if kind == "fsk-burst" else float(p["attenuation_db"]),
                identity={"type": "sensor_id", "value": f"{int(p['sensor_id']):04x}"},
            )
            scene.annotate(s, n, f - bw / 2, f + bw / 2, kind, truth)

    # ---- the decoy: same family, own content ---------------------------------------------
    decoy_off = float(p["decoy_offset_hz"])
    decoy_payload = scene.rng("payload", "decoy")
    decoy_phase = scene.rng("phase", "decoy")
    n_decoy = 0
    for k, t in enumerate(_schedule(scene, p, "decoy")):
        bits = _burst_bits(p, int(p["decoy_sensor_id"]), k, decoy_payload)
        iq = fsk.cpfsk(bits, fs, rate, dev, bt=bt,
                       phase0=float(decoy_phase.uniform(0, 2 * math.pi)))
        start = max(0, int(round(t * fs)))
        placed = place(start, iq, decoy_off, amp)
        if placed is None:
            break
        n_decoy += 1
        f = cap.center_hz + decoy_off
        truth = scene.emission_truth(
            cap, decoy_off, bw, power, kind="independent-station", modulation="2fsk", levels=2,
            symbol_rate_bd=rate, deviation_hz=dev, mod_index=2 * dev / rate, bt=bt,
            burst_index=k, path="independent",
            identity={"type": "sensor_id", "value": f"{int(p['decoy_sensor_id']):04x}"},
        )
        scene.annotate(placed[0], placed[1], f - bw / 2, f + bw / 2, "independent-station", truth)

    scene.scenario_truth["multipath"] = {
        "direct_rf_center_hz": cap.center_hz + direct_off,
        "echo_rf_center_hz": cap.center_hz + echo_off,
        "decoy_rf_center_hz": cap.center_hz + decoy_off,
        "bandwidth_hz": bw,
        "delay_s": float(p["delay_s"]),
        "path_difference_m": float(p["delay_s"]) * 299_792_458.0,
        "attenuation_db": float(p["attenuation_db"]),
        "pair_independent": independent,
        "cadence_s": float(p["cadence_s"]),
        "n_bursts": n_bursts,
        "n_decoy_bursts": n_decoy,
        "aperiodic": {"gap_min_s": float(p["gap_min_s"]), "gap_max_s": float(p["gap_max_s"])},
        "note": ("the two paired channels are TWO INDEPENDENT emitters keying on one cadence a "
                 "fixed phase apart -- identical envelopes, unrelated content, nothing to relate"
                 if independent else
                 "the echo is the same waveform as the direct path, delayed and attenuated; the "
                 "decoy is an independent station of the same family, bandwidth and modulation"),
    }
    return [scene], {}
