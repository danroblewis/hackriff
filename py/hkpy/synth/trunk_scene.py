"""Trunking control-channel scenario (T-267, C23): a continuous C4FM CC hidden among bursty NBFM.

The scene exists to separate two things a frequency-channel-occupancy (FCO) measure cannot:

- a **control channel** — continuous C4FM on the 12.5 kHz LMR raster, carrying a real frame sync
  and CRC-valid blocks;
- a **continuous data emitter** — equally continuous, equally on-raster, equally 4FSK, carrying
  neither.

Both sit at 100 % FCO on the raster, so both are *candidates*. Only the first should ever be
confirmed. That decoy is the point of the fixture: C23's named pitfall is "false CCs: continuous
data emitters pass the FCO test. Require sync plus CRC."

Bursty NBFM neighbours fill the rest of the raster so the CC has to be found among traffic
rather than in an otherwise-empty band.
"""

from __future__ import annotations

import math
from typing import Any

import numpy as np

from hkpy.synth import trunking as tk
from hkpy.synth.scenarios import Ctx, DEFAULT_START_UTC, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, undb

TRUNK_CC_DEFAULTS: dict[str, Any] = {
    "sample_rate": 500e3,
    # 800 MHz public-safety trunking (docs/04 §4: 851-869 MHz, control channels 100 % duty).
    "center_hz": 851.0125e6,
    "duration_s": 1.0,
    "raster_hz": tk.LMR_RASTER_HZ,
    # Raster channel indices, relative to center_hz.
    "cc_channel": 3,
    "decoy_channel": -5,
    "nbfm_channels": [-2, 7, 11, -9],
    "cc_snr_db": 20.0,
    "decoy_snr_db": 20.0,
    "nbfm_snr_db": 18.0,
    "symbol_rate_bd": tk.C4FM_SYMBOL_RATE_BD,
    "cc_cfo_hz": 0.0,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "burst_mean_on_s": 0.08,
    "burst_mean_off_s": 0.12,
    "noise_dbfs": -40.0,
    "calibration_k_db": -70.0,
    "start_utc": DEFAULT_START_UTC,
    # --- TSBK content (T-268). OFF by default, so ``trunk_control_channel`` stays exactly the
    # T-267 scene: the branch below consumes no randomness when it is off, so the recording is
    # byte-for-byte what it was. ``trunk_tsbk_control_channel`` turns it on.
    "tsbk": False,
    # The band plan the control channel announces. 851.00625 MHz base / 6.25 kHz spacing is an
    # 800 MHz public-safety plan; both encode exactly (base is a whole number of 5 Hz steps,
    # spacing a whole number of 125 Hz steps).
    "tsbk_iden": 1,
    "tsbk_base_hz": 851.00625e6,
    "tsbk_spacing_hz": 6250.0,
    "tsbk_tx_offset_hz": -45e6,
    "tsbk_bandwidth_hz": 12500.0,
    # The voice frequency a grant must resolve to. The CHANNEL NUMBER is derived from it, not the
    # other way round, so the truth is a frequency chosen first and the decoder has to arrive at
    # it through the band plan rather than through arithmetic the fixture also did.
    "grant_target_hz": 851.7375e6,
    "grant_talkgroup": 1234,
    "grant_source": 5678,
    # A grant naming an identifier the control channel NEVER announces. Resolving it through the
    # announced identifier would give a perfectly plausible 800 MHz frequency, which is exactly
    # the C23 pitfall; it must come out unmapped instead.
    "unannounced_iden": 7,
    "unannounced_channel": 300,
    # --- The voice channel a grant must be FOLLOWED onto (T-269). Chosen as a frequency first,
    # exactly like ``grant_target_hz``; its channel number is derived from the band plan above, so
    # the decoder still has to arrive at it through the IDEN_UP messages.
    #
    # 851.075 MHz is +62.5 kHz from the tuned centre and so inside the window the radio is holding,
    # while ``grant_target_hz`` at +725 kHz is outside it. One recording therefore carries BOTH
    # C23 cases: a grant that can be followed, and a grant the <=20 MHz span limit refuses.
    "follow_target_hz": 851.075e6,
    "follow_talkgroup": 2468,
    "follow_source": 1357,
    # The traffic on that channel: repeated keyings, each far shorter than the window a hunt
    # buffers, separated by more silence than any silence timeout shorter than a P25 voice frame.
    # on 0.10 + off 0.15 = a 0.25 s period, so ANY 0.5 s window spans two keyings and contains at
    # least one complete one with its closing silence -- which is what lets a follower measure a
    # start AND an end rather than a fragment. At 40 % duty the channel never reaches control-
    # channel candidacy (MIN_CC_FCO is 0.95), so it cannot be mistaken for a second CC.
    "follow_on_s": 0.10,
    "follow_off_s": 0.15,
    "follow_snr_db": 18.0,
}

#: The same scene with TSBK content switched on (T-268).
TRUNK_TSBK_DEFAULTS: dict[str, Any] = {**TRUNK_CC_DEFAULTS, "tsbk": True}


def _tsbk_stream(p: dict[str, Any], n_frames: int) -> tuple[np.ndarray, list[dict[str, Any]], dict[str, Any]]:
    """The control channel's outbound message stream, and the truth describing it.

    The cycle carries, in order: an identifier update, a grant that resolves through it, the same
    identifier update again (a band-plan entry is only trustworthy once it has been repeated), a
    grant update for the same channel, a grant onto a channel that is inside the tuned window
    (T-269), a grant naming an identifier that is never announced, and a message this decoder does
    not read. Repeating the cycle makes every one of those appear several times in any window long
    enough to confirm the channel at all.

    Two of those grants resolve to real frequencies that differ in one way that matters: one lands
    inside the window the radio is holding and one lands outside it. A follower must follow the
    first and refuse -- visibly -- the second.
    """
    iden = int(p["tsbk_iden"])
    base_hz, spacing_hz = float(p["tsbk_base_hz"]), float(p["tsbk_spacing_hz"])

    def channel_of(hz: float, what: str) -> int:
        """The band-plan channel number of a frequency. The FREQUENCY is the truth; this derives
        the number a grant has to carry to name it, never the other way round."""
        steps = (hz - base_hz) / spacing_hz
        if steps != int(steps) or not 0 <= steps <= 0xFFF:
            raise ValueError(f"{what} {hz} is not a channel of this band plan")
        return int(steps)

    target_hz = float(p["grant_target_hz"])
    channel = channel_of(target_hz, "grant_target_hz")
    follow_hz = float(p["follow_target_hz"])
    follow_channel = channel_of(follow_hz, "follow_target_hz")
    if follow_hz == target_hz:
        raise ValueError("the followed channel and the out-of-window channel must differ")
    tg, src = int(p["grant_talkgroup"]), int(p["grant_source"])
    f_tg, f_src = int(p["follow_talkgroup"]), int(p["follow_source"])
    u_iden, u_chan = int(p["unannounced_iden"]), int(p["unannounced_channel"])
    if u_iden == iden:
        raise ValueError("the unannounced identifier must differ from the announced one")

    iden_args = tk.iden_up_args(iden, base_hz, spacing_hz,
                                tx_offset_hz=float(p["tsbk_tx_offset_hz"]),
                                bandwidth_hz=float(p["tsbk_bandwidth_hz"]))
    iden_block = tk.tsbk(tk.TSBK_OP_IDEN_UP, iden_args)
    chan16 = tk.channel_number(iden, channel)
    follow_chan16 = tk.channel_number(iden, follow_channel)
    u_chan16 = tk.channel_number(u_iden, u_chan)
    cycle = [
        ("iden-up", iden_block),
        ("grant", tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(chan16, tg, source=src))),
        ("iden-up", iden_block),
        ("grant-update", tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT_UPDATE, tk.grant_args(chan16, tg))),
        ("grant-follow",
         tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(follow_chan16, f_tg, source=f_src))),
        ("grant-unannounced",
         tk.tsbk(tk.TSBK_OP_GRP_VCH_GRANT, tk.grant_args(u_chan16, tg + 1, source=src))),
        # An opcode this decoder does not read (RFSS status broadcast), so the stream is not made
        # only of messages it happens to understand.
        ("other", tk.tsbk(0x3A, bytes(8))),
    ]
    dibits = tk.frames_from_blocks([b for _, b in cycle], n_frames)
    frames = [{"index": i, "kind": cycle[i % len(cycle)][0],
               "block_hex": cycle[i % len(cycle)][1].hex()} for i in range(n_frames)]
    counts = {kind: sum(1 for i in range(n_frames) if cycle[i % len(cycle)][0] == kind)
              for kind, _ in cycle}
    truth = {
        "iden": iden,
        "base_hz": base_hz,
        "spacing_hz": spacing_hz,
        "tx_offset_hz": float(p["tsbk_tx_offset_hz"]),
        "bandwidth_hz": float(p["tsbk_bandwidth_hz"]),
        "grant_channel": channel,
        "grant_channel_16bit": chan16,
        "grant_target_hz": target_hz,
        "grant_talkgroup": tg,
        "grant_source": src,
        "follow_channel": follow_channel,
        "follow_channel_16bit": follow_chan16,
        "follow_target_hz": follow_hz,
        "follow_talkgroup": f_tg,
        "follow_source": f_src,
        "unannounced_iden": u_iden,
        "unannounced_channel": u_chan,
        "unannounced_channel_16bit": u_chan16,
        # What a decoder that fell back to the announced identifier would produce. Nothing may
        # ever report this frequency.
        "wrong_frequency_if_misresolved_hz": base_hz + spacing_hz * u_chan,
        "frames_per_cycle": len(cycle),
        "counts": counts,
        "expected": {
            "protocol": "p25-phase1",
            "iden_admitted": True,
            "grant_maps_to_target": True,
            "unannounced_grant": "unmapped-channel",
        },
        "coding": "none (no trellis, no interleaving, no status symbols; CRC-16/CCITT-FALSE, "
                  "not the augmented CRC-CCITT real P25 uses)",
    }
    return dibits, frames, truth


def _n(p: dict[str, Any]) -> int:
    return int(round(float(p["duration_s"]) * float(p["sample_rate"])))


def _power(cap: Any, snr_db: float, bw_hz: float) -> float:
    """Absolute dBFS for a wanted SNR in ``bw_hz`` against the capture's floor density."""
    return float(snr_db) + cap.floor_dbfs_per_hz + db(bw_hz)


def _on_off(rng: np.random.Generator, span_s: float, mean_on: float, mean_off: float
            ) -> list[tuple[float, float]]:
    """Two-state on/off intervals over ``span_s`` (exponential dwell times)."""
    out: list[tuple[float, float]] = []
    t = float(rng.exponential(mean_off))
    while t < span_s:
        on = float(rng.exponential(mean_on))
        end = min(span_s, t + on)
        if end > t:
            out.append((t, end))
        t = end + float(rng.exponential(mean_off))
    return out


def trunk_control_channel(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    n = _n(p)
    scene = ctx.scene(
        "trunk_control_channel", fs, n,
        "hkpy.synth trunk_control_channel: continuous C4FM control channel, a continuous 4FSK "
        "decoy with no framing, and bursty NBFM on the 12.5 kHz LMR raster",
    )

    # Noise floor over the whole scene.
    cap = scene.add_capture(0, n, float(p["center_hz"]), utc_plus(p["start_utc"], 0.0),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(scene.rng("noise"), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)

    raster = float(p["raster_hz"])
    rate = float(p["symbol_rate_bd"])
    # C4FM occupies a 12.5 kHz channel; Carson on the outer deviation plus the symbol rate.
    c4fm_bw = 2 * (1800.0 + rate / 2)
    t_all = scene.time(0, n)

    def place_c4fm(channel: int, dibits: np.ndarray, snr_db: float, cfo_hz: float) -> tuple[float, float]:
        off = channel * raster + cfo_hz
        if abs(off) + c4fm_bw / 2 > fs / 2:
            raise ValueError(f"channel {channel} does not fit inside the sample rate")
        power = _power(cap, snr_db, c4fm_bw)
        iq = tk.c4fm(dibits, fs, rate)[:n]
        phase0 = float(scene.rng("phase", channel).uniform(0, 2 * math.pi))
        scene.add_samples(0, math.sqrt(undb(power)) * iq
                          * np.exp(1j * (2 * math.pi * off * t_all[: len(iq)] + phase0)))
        return off, power

    # --- The control channel: continuous C4FM, real frame sync, CRC-valid blocks.
    frames_needed = int(math.ceil(n * rate / fs / tk.FRAME_DIBITS)) + 1
    tsbk_truth: dict[str, Any] | None = None
    if bool(p["tsbk"]):
        cc_dibits, cc_frames, tsbk_truth = _tsbk_stream(p, frames_needed)
    else:
        cc_dibits, cc_frames = tk.control_channel_dibits(scene.rng("cc"), frames_needed)
    cc_ch = int(p["cc_channel"])
    cc_off, cc_power = place_c4fm(cc_ch, cc_dibits, float(p["cc_snr_db"]), float(p["cc_cfo_hz"]))
    cc_f = cap.center_hz + cc_off
    scene.annotate(
        0, n, cc_f - c4fm_bw / 2, cc_f + c4fm_bw / 2, "trunk-control-channel",
        scene.emission_truth(
            cap, cc_off, c4fm_bw, cc_power,
            kind="trunk-control-channel", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=cc_ch,
            nominal_center_hz=cap.center_hz + cc_ch * raster,
            cfo_hz=float(p["cc_cfo_hz"]),
            is_control_channel=True, confirmable=True,
            frame=tk.CC_FRAME_SPEC,
            n_frames=len(cc_frames),
            frames=cc_frames[:8],
            sync_hex=tk.P25_FRAME_SYNC_HEX,
            carries_tsbk=tsbk_truth is not None,
        ),
    )

    # --- The decoy: continuous, on-raster, 4FSK, and unframed. Must never be confirmed.
    n_dibits = int(math.ceil(n * rate / fs)) + 1
    decoy_dibits = tk.continuous_data_dibits(scene.rng("decoy"), n_dibits)
    dc_ch = int(p["decoy_channel"])
    dc_off, dc_power = place_c4fm(dc_ch, decoy_dibits, float(p["decoy_snr_db"]), 0.0)
    dc_f = cap.center_hz + dc_off
    scene.annotate(
        0, n, dc_f - c4fm_bw / 2, dc_f + c4fm_bw / 2, "continuous-data",
        scene.emission_truth(
            cap, dc_off, c4fm_bw, dc_power,
            kind="continuous-data", modulation="c4fm", levels=4,
            symbol_rate_bd=rate, duty_cycle=1.0, fco=1.0,
            raster_hz=raster, raster_channel=dc_ch,
            nominal_center_hz=cap.center_hz + dc_ch * raster,
            is_control_channel=False, confirmable=False,
            why_not="continuous 4FSK with no frame sync and no CRC-valid block: passes FCO "
                    "candidacy, fails sync+CRC confirmation",
        ),
    )

    # --- Bursty NBFM neighbours.
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    nbfm_bw = 2 * (dev + tone)
    span_s = n / fs
    n_bursts = 0
    for ch in [int(c) for c in p["nbfm_channels"]]:
        off = ch * raster
        if abs(off) + nbfm_bw / 2 > fs / 2:
            raise ValueError(f"nbfm channel {ch} does not fit inside the sample rate")
        power = _power(cap, float(p["nbfm_snr_db"]), nbfm_bw)
        amp = math.sqrt(undb(power))
        r = scene.rng("nbfm", ch)
        for b0, b1 in _on_off(r, span_s, float(p["burst_mean_on_s"]), float(p["burst_mean_off_s"])):
            i0, i1 = int(round(b0 * fs)), min(n, int(round(b1 * fs)))
            if i1 <= i0:
                continue
            phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
            tt = scene.time(i0, i1 - i0)
            phase = (phase0 + 2 * math.pi * off * tt
                     + (dev / tone) * np.sin(2 * math.pi * tone * (tt - b0) + audio_phase))
            scene.add_samples(i0, amp * np.exp(1j * phase))
            f = cap.center_hz + off
            scene.annotate(
                i0, i1 - i0, f - nbfm_bw / 2, f + nbfm_bw / 2, "nbfm-burst",
                scene.emission_truth(
                    cap, off, nbfm_bw, power, kind="nbfm-burst", modulation="nbfm",
                    deviation_hz=dev, audio_tone_hz=tone,
                    raster_hz=raster, raster_channel=ch,
                    is_control_channel=False, confirmable=False,
                    burst_start_s=b0, burst_duration_s=b1 - b0,
                ),
            )
            n_bursts += 1

    # --- The voice channel a grant is followed onto (T-269), present only when the control
    # channel is actually issuing grants. Bursty by construction: repeated keyings with real
    # silence between them, so a follower has boundaries to **measure** rather than a carrier that
    # is simply always there. At 40 % duty it never reaches control-channel candidacy
    # (MIN_CC_FCO is 0.95), so it cannot be mistaken for a second control channel.
    if tsbk_truth is not None:
        v_off = float(tsbk_truth["follow_target_hz"]) - cap.center_hz
        if abs(v_off) + c4fm_bw / 2 > fs / 2:
            raise ValueError("follow_target_hz does not fit inside the sample rate")
        v_power = _power(cap, float(p["follow_snr_db"]), c4fm_bw)
        v_amp = math.sqrt(undb(v_power))
        on_s, off_s = float(p["follow_on_s"]), float(p["follow_off_s"])
        if on_s <= 0 or off_s <= 0:
            raise ValueError("a keying needs a positive on and off time")
        r = scene.rng("voice")
        keyings: list[tuple[float, float]] = []
        t_key = 0.0
        while t_key < span_s:
            i0, i1 = int(round(t_key * fs)), min(n, int(round((t_key + on_s) * fs)))
            if i1 > i0:
                n_dibits = int(math.ceil((i1 - i0) * rate / fs)) + 1
                iq = tk.c4fm(tk.continuous_data_dibits(r, n_dibits), fs, rate)[: i1 - i0]
                phase0 = float(r.uniform(0, 2 * math.pi))
                tt = scene.time(i0, len(iq))
                scene.add_samples(
                    i0, v_amp * iq * np.exp(1j * (2 * math.pi * v_off * tt + phase0)))
                b0, b1 = i0 / fs, (i0 + len(iq)) / fs
                keyings.append((b0, b1))
                vf = cap.center_hz + v_off
                scene.annotate(
                    i0, len(iq), vf - c4fm_bw / 2, vf + c4fm_bw / 2, "trunk-voice-keying",
                    scene.emission_truth(
                        cap, v_off, c4fm_bw, v_power,
                        kind="trunk-voice-keying", modulation="c4fm", levels=4,
                        symbol_rate_bd=rate, raster_hz=raster,
                        raster_channel=int(round(v_off / raster)),
                        is_control_channel=False, confirmable=False,
                        burst_start_s=b0, burst_duration_s=b1 - b0,
                        granted_by_channel_16bit=tsbk_truth["follow_channel_16bit"],
                        talkgroup=tsbk_truth["follow_talkgroup"],
                    ),
                )
            t_key += on_s + off_s
        tsbk_truth.update({
            "follow_on_s": on_s,
            "follow_off_s": off_s,
            "follow_period_s": on_s + off_s,
            "follow_snr_db": float(p["follow_snr_db"]),
            "follow_offset_hz": v_off,
            "follow_keyings_s": [[a, b] for a, b in keyings],
            # Where each resolved grant falls relative to the window the radio is holding. The
            # follow target is inside it; the T-268 target is outside, which is the C23 <=20 MHz
            # span limit a follower must REPORT rather than drop.
            "grant_target_offset_hz": float(tsbk_truth["grant_target_hz"]) - cap.center_hz,
            "sample_rate_hz": fs,
        })
        tsbk_truth["expected"]["follow_grant"] = "followed: a call record with measured boundaries"
        tsbk_truth["expected"]["grant_target"] = "outside-window: logged, never followed"

    scene.scenario_truth["trunking"] = {
        "raster_hz": raster,
        "raster_origin_hz": cap.center_hz,
        "control_channel": {
            "rf_center_hz": cc_f,
            "raster_channel": cc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "sync_hex": tk.P25_FRAME_SYNC_HEX,
            "n_frames": len(cc_frames),
            "expected_confirmed": True,
        },
        "continuous_decoy": {
            "rf_center_hz": dc_f,
            "raster_channel": dc_ch,
            "modulation": "c4fm",
            "symbol_rate_bd": rate,
            "bandwidth_hz": c4fm_bw,
            "duty_cycle": 1.0,
            "expected_confirmed": False,
            "why_not": "no frame sync, no CRC-valid block",
        },
        "nbfm_channels": [int(c) for c in p["nbfm_channels"]],
        "n_nbfm_bursts": n_bursts,
        "frame": tk.CC_FRAME_SPEC,
    }
    if tsbk_truth is not None:
        scene.scenario_truth["trunking"]["tsbk"] = tsbk_truth
    return [scene], {}
