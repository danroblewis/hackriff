"""LoRa chirps in the 915 MHz US ISM band (T-255): a signal with no stable frequency at all.

**The band is 902-928 MHz US ISM.** There is a 2.4 GHz LoRa variant; this is not it. 902-928 is
where CLAUDE.md's invariants point ("the canonical playground: short bursts everywhere"), and it
is where the receiver story - wideband, unlicensed, crowded with short emissions - matches what
the fixture is for.

**The scene is a contrast, not a single signal.** Invariant 1 makes two independent claims about a
detection: that it has a *time extent*, and that it needs *no stable frequency*. One emitter can
only test one of them, so the scene carries three species that separate the two axes:

==================  ==================  =====================  =================================
species             stable frequency?   bounded time extent?   what it tests
==================  ==================  =====================  =================================
``cw`` carrier      yes                 no (whole recording)   the "steady emitter parked on one
                                                               frequency" shape invariant 1 says
                                                               nothing must be forced into
``fsk-burst``       yes                 yes (~17 ms)           a time extent alone is not enough
``lora-packet``     **no**              yes (~130 ms)          both at once - the case the model
                                                               is actually being asked about
==================  ==================  =====================  =================================

A detector that models a carrier which happens to persist can pass on the first two and still fail
completely on the third.

**ADR-0017 §1.3, stated in the truth rather than discovered later.** One ``(f_lo, f_hi)`` per
detection cannot describe a swept carrier, so the LoRa annotation's frequency box is the *bounding
box of the sweep* and says so (``box_is_bounding_box``). The truth also carries the polyline that
box is standing in for (``sweep_polyline``) and the ratio between the box width and what the
emission actually occupies at any instant (``sweep.box_to_instantaneous_ratio``), so the cost of
the rectangle is a number a test can assert on instead of a caveat in a document.

Hidden ground truth: spreading factor, bandwidth, coding rate and payload live in the annotations
only. Nothing the pipeline reads names them - the blind harness strips every annotation before the
mock SDR opens the recording.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import fsk, lora
from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

LORA_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    #: 903.0 MHz: inside 902-928 US ISM, with 903.1 (a 125 kHz US915 uplink channel) in the window.
    "center_hz": 903.0e6,
    "duration_s": 0.5,
    # --- the chirp
    "lora_offset_hz": 100e3,        # -> 903.1 MHz
    "sf": 9,
    "bandwidth_hz": 125e3,
    "coding_rate": 1,               # 4/(4+cr): 1 -> 4/5
    "payload_bytes": 12,
    "payload_hex": "",              # empty: drawn from the seed
    "preamble_symbols": lora.DEFAULT_PREAMBLE_SYMBOLS,
    "sync_word": lora.PUBLIC_SYNC_WORD,
    "n_packets": 2,
    "first_packet_s": 0.04,
    "packet_period_s": 0.24,
    "lora_snr_db": 12.0,
    "lora_cfo_hz": 1200.0,
    "polyline_step_s": 1e-3,
    #: Analysis frame the quoted instantaneous bandwidth refers to: 512 bins at 500 kHz. The ratio
    #: it produces is not a property of this frame in particular - box / instantaneous is exactly
    #: ``symbol_duration_s / frame_s``, so any frame can be worked out from the truth.
    "instantaneous_frame_s": 1.024e-3,
    # --- the stable-frequency contrasts
    "cw_offset_hz": -180e3,
    #: SNR in ``cw_reference_bw_hz``, not in a bin: a carrier has no bandwidth of its own, so the
    #: reference has to be stated or the absolute power is meaningless.
    "cw_snr_db": 25.0,
    "cw_reference_bw_hz": 1e3,
    "fsk_offset_hz": -60e3,
    "fsk_snr_db": 16.0,
    "fsk_symbol_rate_bd": 4800.0,
    "fsk_deviation_hz": 9600.0,
    "n_fsk_bursts": 3,
    "first_fsk_s": 0.02,
    "fsk_period_s": 0.15,
    "fsk_sync_hex": "2dd4",
    "fsk_sensor_id": 0x7E21,
    # --- receiver
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
}


def _power(cap: Any, snr_db: float, bw_hz: float) -> float:
    """Absolute dBFS for a wanted SNR in ``bw_hz`` against the capture's floor density."""
    return float(snr_db) + cap.floor_dbfs_per_hz + db(bw_hz)


def _fits(offset_hz: float, bw_hz: float, fs: float, what: str) -> None:
    if abs(offset_hz) + bw_hz / 2 > fs / 2:
        raise ValueError(f"{what} at {offset_hz:+.0f} Hz ({bw_hz:.0f} Hz wide) leaves the window")


def _polyline(freq: np.ndarray, fs: float, t0: float, f_center: float,
              step_s: float) -> list[list[float]]:
    """``[[t_s, f_hz], ...]`` of the true instantaneous frequency, absolute Hz.

    This is exactly the object ADR-0017 §1.3 says the model cannot yet draw. Carrying it in truth
    costs a few kB and means the limitation is measurable rather than merely acknowledged.
    """
    stride = max(1, int(round(step_s * fs)))
    idx = np.arange(0, len(freq), stride)
    return [[float(t0 + i / fs), float(f_center + freq[i])] for i in idx]


def lora_ism_burst(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    n = int(round(float(p["duration_s"]) * fs))
    scene = ctx.scene(
        "lora_ism_burst", fs, n,
        "hkpy.synth lora_ism_burst: LoRa CSS up-chirp packets in 902-928 MHz US ISM, beside a "
        "steady CW carrier and short fixed-frequency FSK bursts",
    )
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    sf = int(p["sf"])
    bw = float(p["bandwidth_hz"])
    cr = int(p["coding_rate"])
    lora_off = float(p["lora_offset_hz"]) + float(p["lora_cfo_hz"])
    _fits(lora_off, bw, fs, "the LoRa channel")
    t_sym = lora.symbol_duration_s(sf, bw)
    slope = lora.chirp_rate_hz_per_s(sf, bw)
    frame_s = float(p["instantaneous_frame_s"])
    inst_bw = lora.instantaneous_bandwidth_hz(sf, bw, frame_s)
    lora_power = _power(cap, float(p["lora_snr_db"]), bw)
    lora_amp = math.sqrt(undb(lora_power))
    t_all = scene.time(0, n)

    fixed_payload = bytes.fromhex(str(p["payload_hex"])) if p["payload_hex"] else None
    payload_rng = scene.rng("payload")
    packets: list[dict[str, Any]] = []
    for k in range(int(p["n_packets"])):
        payload = fixed_payload if fixed_payload is not None else bytes(
            int(v) for v in payload_rng.integers(0, 256, int(p["payload_bytes"])))
        symbols, detail = lora.encode(payload, sf, cr)
        freq = lora.packet_frequency(symbols, sf, bw, fs,
                                     preamble_symbols=int(p["preamble_symbols"]),
                                     sync_word=int(p["sync_word"]))
        phase0 = float(scene.rng("phase", k).uniform(0, 2 * math.pi))
        iq = np.exp(1j * (phase0 + 2 * math.pi * np.cumsum(freq) / fs))
        start = int(round((float(p["first_packet_s"]) + k * float(p["packet_period_s"])) * fs))
        if start < 0 or start + len(iq) > n:
            raise ValueError(f"LoRa packet {k} ({len(iq) / fs:.3f} s) does not fit in the recording")
        tt = t_all[start : start + len(iq)]
        scene.add_samples(start, lora_amp * iq * np.exp(2j * math.pi * lora_off * tt))

        f_center = cap.center_hz + lora_off
        duration_s = len(iq) / fs
        truth = scene.emission_truth(
            cap, lora_off, bw, lora_power,
            kind="lora-packet", modulation="lora-css", family_hint="css",
            nominal_center_hz=cap.center_hz + float(p["lora_offset_hz"]),
            cfo_hz=float(p["lora_cfo_hz"]),
            spreading_factor=sf, chips_per_symbol=lora.n_chips(sf),
            coding_rate=f"4/{4 + cr}", coding_rate_index=cr,
            symbol_duration_s=t_sym, symbol_rate_bd=1.0 / t_sym,
            chip_rate_hz=bw, bits_per_symbol=sf,
            preamble_symbols=int(p["preamble_symbols"]),
            sync_word=f"0x{int(p['sync_word']):02x}",
            sfd_symbols=lora.SFD_SYMBOLS,
            packet_index=k,
            n_symbols_total=int(round(duration_s / t_sym * 1000) / 1000),
            payload_symbols=[int(s) for s in symbols],
            frame=detail,
            identity={"type": "lora_payload", "value": payload.hex()},
            sweep={
                "shape": "linear up-chirp, cyclically folded once per symbol",
                "chirp_rate_hz_per_s": slope,
                "sweeps_per_second": 1.0 / t_sym,
                "f_low_hz": f_center - bw / 2,
                "f_high_hz": f_center + bw / 2,
                "stable_frequency": False,
                "instantaneous_bandwidth_hz": inst_bw,
                "instantaneous_frame_s": frame_s,
                "box_to_instantaneous_ratio": bw / inst_bw,
                "ratio_identity": "box / instantaneous = symbol_duration_s / frame_s, so the "
                                  "waste grows with the spreading factor: 4x at SF9 and 32x at "
                                  "SF12 for this frame",
                "box_is_bounding_box": True,
                "limitation": "ADR-0017 §1.3: one (f_lo, f_hi) per detection cannot describe a "
                              "swept carrier, so this box is the bounding box of the sweep and "
                              "not the sweep itself; sweep_polyline is what it stands in for",
            },
            sweep_polyline=_polyline(freq, fs, start / fs, f_center, float(p["polyline_step_s"])),
            polyline_step_s=float(p["polyline_step_s"]),
        )
        scene.annotate(start, len(iq), f_center - bw / 2, f_center + bw / 2, "lora-packet", truth)
        packets.append({
            "packet_index": k,
            "t_start_s": start / fs,
            "duration_s": duration_s,
            "payload_hex": payload.hex(),
            "crc_hex": detail["crc_hex"],
            "n_payload_symbols": int(len(symbols)),
        })

    # --- Contrast 1: a steady CW carrier for the whole recording. Stable frequency, no bounded
    # time extent: the shape invariant 1 says nothing must be forced into.
    cw_off = float(p["cw_offset_hz"])
    _fits(cw_off, 0.0, fs, "the CW carrier")
    cw_ref_bw = float(p["cw_reference_bw_hz"])
    cw_power = _power(cap, float(p["cw_snr_db"]), cw_ref_bw)
    cw_phase = float(scene.rng("cw").uniform(0, 2 * math.pi))
    scene.add_samples(0, math.sqrt(undb(cw_power))
                      * np.exp(1j * (2 * math.pi * cw_off * t_all + cw_phase)))
    cw_f = cap.center_hz + cw_off
    scene.annotate(0, n, cw_f, cw_f, "cw",
                   scene.emission_truth(cap, cw_off, 0.0, cw_power, kind="cw", modulation="cw",
                                        amplitude=math.sqrt(undb(cw_power)), phase_rad=cw_phase,
                                        snr_db_per_hz=cw_power - cap.floor_dbfs_per_hz,
                                        reference_bandwidth_hz=cw_ref_bw,
                                        snr_in_reference_db=float(p["cw_snr_db"]),
                                        stable_frequency=True, persistent=True,
                                        contrast_role="stable frequency, unbounded time extent"))

    # --- Contrast 2: short fixed-frequency FSK bursts (the 915 MHz remote/sensor traffic that
    # fills this band). Bounded time extent, stable frequency: a time extent alone is not enough.
    rate = float(p["fsk_symbol_rate_bd"])
    dev = float(p["fsk_deviation_hz"])
    fsk_bw = 2 * dev + rate
    fsk_off = float(p["fsk_offset_hz"])
    _fits(fsk_off, fsk_bw, fs, "the FSK channel")
    fsk_power = _power(cap, float(p["fsk_snr_db"]), fsk_bw)
    fsk_amp = math.sqrt(undb(fsk_power))
    sync = bytes.fromhex(str(p["fsk_sync_hex"]))
    sensor_id = int(p["fsk_sensor_id"]) & 0xFFFF
    preamble = np.array([(i + 1) % 2 for i in range(16)], dtype=np.uint8)
    burst_rng = scene.rng("fsk")
    n_bursts = 0
    for k in range(int(p["n_fsk_bursts"])):
        payload = ((sensor_id << 16) | ((k & 0xFF) << 8) | 0x5A).to_bytes(4, "big")
        crc = fsk.crc16_ccitt_false(payload)
        bits = np.concatenate([preamble, fsk.bytes_to_bits(sync), fsk.bytes_to_bits(payload),
                               fsk.bytes_to_bits(crc.to_bytes(2, "big"))])
        iq = fsk.cpfsk(bits, fs, rate, dev, phase0=float(burst_rng.uniform(0, 2 * math.pi)))
        start = int(round((float(p["first_fsk_s"]) + k * float(p["fsk_period_s"])) * fs))
        if start + len(iq) > n:
            break
        tt = t_all[start : start + len(iq)]
        scene.add_samples(start, fsk_amp * iq * np.exp(2j * math.pi * fsk_off * tt))
        f = cap.center_hz + fsk_off
        scene.annotate(
            start, len(iq), f - fsk_bw / 2, f + fsk_bw / 2, "fsk-burst",
            scene.emission_truth(
                cap, fsk_off, fsk_bw, fsk_power, kind="fsk-burst", modulation="2fsk", levels=2,
                symbol_rate_bd=rate, deviation_hz=dev, mod_index=2 * dev / rate, bt=0.0,
                nominal_center_hz=f, cfo_hz=0.0, burst_index=k,
                bit_order="msb-first", mapping="bit 1 = +deviation_hz",
                stable_frequency=True, persistent=False,
                contrast_role="stable frequency, bounded time extent",
                frame={"n_bits": int(len(bits)), "bits_hex": fsk.bits_to_hex(bits),
                       "preamble_bits": int(len(preamble)),
                       "preamble_hex": fsk.bits_to_hex(preamble), "sync_hex": sync.hex(),
                       "payload_hex": payload.hex(), "crc_hex": f"{crc:04x}"},
                payload_fields={"sensor_id": sensor_id, "seq": k & 0xFF},
                crc={**fsk.CRC16_SPEC, "value": f"0x{crc:04X}", "valid": True},
                identity={"type": "sensor_id", "value": f"{sensor_id:04x}"},
            ),
        )
        n_bursts += 1

    scene.scenario_truth["lora"] = {
        "band": "902-928 MHz US ISM (NOT the 2.4 GHz LoRa variant)",
        "rf_center_hz": cap.center_hz + float(p["lora_offset_hz"]),
        "spreading_factor": sf,
        "bandwidth_hz": bw,
        "coding_rate": f"4/{4 + cr}",
        "coding_rate_index": cr,
        "symbol_duration_s": t_sym,
        "chirp_rate_hz_per_s": slope,
        "instantaneous_bandwidth_hz": inst_bw,
        "instantaneous_frame_s": frame_s,
        "box_to_instantaneous_ratio": bw / inst_bw,
        "preamble_symbols": int(p["preamble_symbols"]),
        "sync_word": f"0x{int(p['sync_word']):02x}",
        "packets": packets,
        "coding": lora.CODING_SPEC,
        "crc": lora.CRC16_SPEC,
    }
    scene.scenario_truth["contrast"] = {
        "cw": {"rf_center_hz": cw_f, "stable_frequency": True, "persistent": True},
        "fsk": {"rf_center_hz": cap.center_hz + fsk_off, "bandwidth_hz": fsk_bw,
                "n_bursts": n_bursts, "burst_duration_s": len(iq) / fs if n_bursts else 0.0,
                "stable_frequency": True, "persistent": False},
        "claim": "a chirp is the only one of the three with no stable frequency; the CW carrier is "
                 "the only one with no bounded time extent",
    }
    return [scene], {}
