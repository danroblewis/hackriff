"""``occupancy_multi_hour`` (AWARE-042): multi-hour channel occupancy without multi-hour IQ.

**Representation choice.** Hours of IQ are too large to store (3 h at 200 kSps ci8 is ~4.3 GB), so
the truth is a seeded **burst schedule** (``schedule.json``): every burst's channel, start and
duration over the whole span, plus occupancy statistics computed exactly from it. IQ is rendered
**on demand** for short windows (``window_NN.sigmf-*``). Rendering is a pure function of (seed,
schedule, window start): a burst's carrier phase and audio phase depend only on its index and on
absolute schedule time, so a burst that spans two windows renders identically in both. Pass
``render_start_s`` to render one specific window. Pipeline history tests (C12/C26) can take
detections straight from the schedule; detection-path tests replay the rendered windows.

Schedule model per channel: a non-homogeneous Poisson process (thinning) with rate
``rate_per_hour × hour_profile[UTC hour of day]`` (profile normalised to mean 1); durations
log-normal (median ``burst_median_s``, sigma ``burst_sigma``) clipped to
[``burst_min_s``, ``burst_max_s``]; a burst starting within ``min_gap_s`` of the previous burst's end
on the same channel is dropped (so the channel never overlaps itself). Bursts are NBFM with one
audio tone.
"""

from __future__ import annotations

import datetime as _dt
import math
from typing import Any

import numpy as np

from hkpy.synth.scenarios import Ctx, utc_plus
from hkpy.synth.scene import Scene, complex_noise, db, parse_utc, rng_for, undb

#: Relative activity per UTC hour of day (quiet night, busy day), normalised to mean 1 at use.
DIURNAL_PROFILE = [0.2, 0.15, 0.1, 0.1, 0.15, 0.3, 0.6, 1.0, 1.3, 1.4, 1.4, 1.5,
                   1.5, 1.5, 1.5, 1.4, 1.4, 1.5, 1.4, 1.2, 1.0, 0.7, 0.5, 0.3]
DURATION_HISTOGRAM_EDGES_S = [0.0, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 60.0, math.inf]

OCCUPANCY_DEFAULTS: dict[str, Any] = {
    "hours": 3.0,
    "start_utc": "2026-09-13T06:00:00Z",
    "sample_rate": 200e3,
    "center_hz": 446.05e6,
    "first_channel_hz": 446.00625e6,
    "channels": 8,
    "channel_spacing_hz": 12500.0,
    "rates_per_hour": [],
    "rate_min_per_hour": 5.0,
    "rate_max_per_hour": 60.0,
    "hour_profile": [],
    "burst_median_s": 4.0,
    "burst_sigma": 0.8,
    "burst_min_s": 0.2,
    "burst_max_s": 60.0,
    "min_gap_s": 0.5,
    "power_dbfs_min": -35.0,
    "power_dbfs_max": -20.0,
    "fm_deviation_hz": 2500.0,
    "audio_tone_hz": 1000.0,
    "noise_dbfs": -40.0,
    "windows": 2,
    "window_duration_s": 0.25,
    "window_starts_s": [],
    "render_start_s": None,
    "calibration_k_db": -70.0,
}


def _seconds_of_day(t: _dt.datetime) -> float:
    return t.hour * 3600 + t.minute * 60 + t.second + t.microsecond / 1e6


def build_schedule(seed: int, p: dict[str, Any]) -> dict[str, Any]:
    span = float(p["hours"]) * 3600.0
    start = parse_utc(p["start_utc"])
    sod0 = _seconds_of_day(start)
    profile = np.asarray(p["hour_profile"] or DIURNAL_PROFILE, dtype=np.float64)
    if profile.shape != (24,) or np.any(profile < 0) or profile.sum() <= 0:
        raise ValueError("hour_profile needs 24 non-negative values")
    profile = profile / profile.mean()
    n_ch = int(p["channels"])
    ch_rng = rng_for(seed, "occupancy", "channels")
    rates = list(p["rates_per_hour"])
    if rates and len(rates) not in (1, n_ch):
        raise ValueError(f"rates_per_hour needs 0, 1 or {n_ch} values")
    channels = []
    bursts: list[dict[str, Any]] = []
    for c in range(n_ch):
        rate = float(rates[c if len(rates) > 1 else 0]) if rates else \
            float(ch_rng.uniform(p["rate_min_per_hour"], p["rate_max_per_hour"]))
        power = float(ch_rng.uniform(p["power_dbfs_min"], p["power_dbfs_max"]))
        center = float(p["first_channel_hz"]) + c * float(p["channel_spacing_hz"])
        channels.append({"channel": c, "center_hz": center, "rate_per_hour": rate,
                         "power_dbfs": power, "identity": {"type": "channel-user", "value": f"ch{c}"}})
        r = rng_for(seed, "occupancy", "bursts", c)
        lam_max = rate * profile.max() / 3600.0
        t, end_prev = 0.0, -math.inf
        while lam_max > 0:
            t += float(r.exponential(1.0 / lam_max))
            if t >= span:
                break
            hod = int(((sod0 + t) % 86400) // 3600)
            accept = float(r.uniform()) < profile[hod] / profile.max()
            dur = float(np.clip(r.lognormal(math.log(p["burst_median_s"]), p["burst_sigma"]),
                                p["burst_min_s"], p["burst_max_s"]))
            if not accept or t < end_prev + float(p["min_gap_s"]):
                continue
            start_s = round(t, 6)
            dur = round(min(dur, span - start_s), 6)
            if dur <= 0:
                continue
            bursts.append({"channel": c, "start_s": start_s, "duration_s": dur})
            end_prev = start_s + dur
    bursts.sort(key=lambda b: (b["start_s"], b["channel"]))
    for i, b in enumerate(bursts):
        b["index"] = i
    return {
        "kind": "occupancy-schedule",
        "start_utc": p["start_utc"],
        "span_s": span,
        "hour_profile": profile.tolist(),
        "model": __doc__.split("Schedule model per channel: ")[1].strip() if __doc__ else "",
        "channels": channels,
        "bursts": bursts,
        "stats": occupancy_stats(bursts, n_ch, span, p["start_utc"]),
    }


def _overlap(a0: float, a1: float, b0: float, b1: float) -> float:
    return max(0.0, min(a1, b1) - max(a0, b0))


def occupancy_stats(bursts: list[dict[str, Any]], n_channels: int, span_s: float,
                    start_utc: str) -> dict[str, Any]:
    """Exact statistics of a schedule (interval arithmetic, no sampling)."""
    sod0 = _seconds_of_day(parse_utc(start_utc))
    n_hours = int(math.ceil(span_s / 3600.0))
    per_channel = []
    for c in range(n_channels):
        mine = [b for b in bursts if b["channel"] == c]
        durs = np.array([b["duration_s"] for b in mine], dtype=np.float64)
        on = float(durs.sum())
        window_hours = []
        for h in range(n_hours):
            h0, h1 = h * 3600.0, min((h + 1) * 3600.0, span_s)
            busy = sum(_overlap(b["start_s"], b["start_s"] + b["duration_s"], h0, h1) for b in mine)
            window_hours.append({"hour": h, "start_s": h0, "exposure_s": h1 - h0, "on_s": busy,
                                 "occupancy": busy / (h1 - h0),
                                 "n_bursts_started": sum(1 for b in mine if h0 <= b["start_s"] < h1)})
        utc_hours: dict[int, dict[str, float]] = {}
        edge = 0.0
        while edge < span_s:
            hod = int(((sod0 + edge) % 86400) // 3600)
            nxt = min(span_s, edge + (3600.0 - (sod0 + edge) % 3600.0))
            slot = utc_hours.setdefault(hod, {"exposure_s": 0.0, "on_s": 0.0})
            slot["exposure_s"] += nxt - edge
            slot["on_s"] += sum(_overlap(b["start_s"], b["start_s"] + b["duration_s"], edge, nxt)
                                for b in mine)
            edge = nxt
        per_channel.append({
            "channel": c,
            "n_bursts": len(mine),
            "on_time_s": on,
            "occupancy_fraction": on / span_s,
            "duration_mean_s": float(durs.mean()) if len(durs) else None,
            "duration_quantiles_s": {
                q: float(np.percentile(durs, int(q[1:]))) if len(durs) else None
                for q in ("p10", "p50", "p90")
            },
            "window_hours": window_hours,
            "utc_hour_of_day": {str(h): {**v, "occupancy": v["on_s"] / v["exposure_s"]}
                                for h, v in sorted(utc_hours.items())},
        })
    all_durs = np.array([b["duration_s"] for b in bursts], dtype=np.float64)
    hist, _ = np.histogram(all_durs, bins=DURATION_HISTOGRAM_EDGES_S)
    return {
        "span_s": span_s,
        "n_bursts": len(bursts),
        "per_channel": per_channel,
        "duration_histogram": {"edges_s": [e if math.isfinite(e) else None for e in DURATION_HISTOGRAM_EDGES_S],
                               "counts": hist.tolist()},
        "bursts_started_per_window_hour": [
            sum(1 for b in bursts if h * 3600.0 <= b["start_s"] < (h + 1) * 3600.0) for h in range(n_hours)
        ],
    }


def render_window(ctx: Ctx, schedule: dict[str, Any], index: int, start_s: float) -> Scene:
    """IQ for ``[start_s, start_s + window_duration_s)`` of the schedule, with truth annotations."""
    p = ctx.params
    fs = float(p["sample_rate"])
    dur = float(p["window_duration_s"])
    n = int(round(dur * fs))
    g0 = int(round(start_s * fs))  # global sample index of the window start
    scene = ctx.scene(f"window_{index:02d}", fs, n,
                      f"hkpy.synth occupancy_multi_hour window {index} at t={start_s:.6f} s")
    fc = float(p["center_hz"])
    cap = scene.add_capture(0, n, fc, utc_plus(schedule["start_utc"], g0 / fs),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(rng_for(ctx.seed, "occupancy", "noise", g0), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)
    w0, w1 = g0 / fs, (g0 + n) / fs
    dev, tone = float(p["fm_deviation_hz"]), float(p["audio_tone_hz"])
    bw = 2 * (dev + tone)
    in_window = []
    for b in schedule["bursts"]:
        b0, b1 = b["start_s"], b["start_s"] + b["duration_s"]
        if b1 <= w0 or b0 >= w1:
            continue
        ch = schedule["channels"][b["channel"]]
        off = ch["center_hz"] - fc
        i0 = max(0, int(math.ceil(b0 * fs)) - g0)
        i1 = min(n, int(math.ceil(b1 * fs)) - g0)
        if i1 <= i0:
            continue
        r = rng_for(ctx.seed, "occupancy", "burst", b["index"])
        phase0, audio_phase = (float(v) for v in r.uniform(0, 2 * math.pi, 2))
        t_abs = (g0 + np.arange(i0, i1)) / fs
        phase = phase0 + 2 * math.pi * off * t_abs + (dev / tone) * np.sin(2 * math.pi * tone * (t_abs - b0) + audio_phase)
        scene.add_samples(i0, math.sqrt(undb(ch["power_dbfs"])) * np.exp(1j * phase))
        scene.annotate(i0, i1 - i0, ch["center_hz"] - bw / 2, ch["center_hz"] + bw / 2, "nbfm-burst",
                       scene.emission_truth(
                           cap, off, bw, ch["power_dbfs"], kind="nbfm-burst", modulation="nbfm",
                           deviation_hz=dev, audio_tone_hz=tone, channel=b["channel"],
                           burst_index=b["index"], burst_start_s=b0, burst_duration_s=b["duration_s"],
                           clipped_by_window=bool(b0 < w0 or b1 > w1), identity=ch["identity"]))
        in_window.append(b["index"])
    scene.scenario_truth["window"] = {"index": index, "start_s": w0, "duration_s": n / fs,
                                      "start_utc": cap.datetime, "bursts": in_window,
                                      "schedule_file": "schedule.json"}
    return scene


def occupancy_multi_hour(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    fs = float(p["sample_rate"])
    for c in range(int(p["channels"])):
        off = float(p["first_channel_hz"]) + c * float(p["channel_spacing_hz"]) - float(p["center_hz"])
        if abs(off) > 0.45 * fs:
            raise ValueError(f"channel {c} at offset {off} Hz is outside the rendered bandwidth")
    schedule = build_schedule(ctx.seed, p)
    dur = float(p["window_duration_s"])
    span = schedule["span_s"]
    if p["render_start_s"] is not None:
        starts = [float(p["render_start_s"])]
    elif p["window_starts_s"]:
        starts = [float(s) for s in p["window_starts_s"]]
    else:
        r = rng_for(ctx.seed, "occupancy", "windows")
        bursts = schedule["bursts"]
        starts = []
        for _ in range(int(p["windows"])):
            if bursts:
                b = bursts[int(r.integers(len(bursts)))]
                starts.append(min(max(0.0, b["start_s"] - 0.05), span - dur))
            else:
                starts.append(float(r.uniform(0, span - dur)))
    for s in starts:
        if not 0 <= s <= span - dur:
            raise ValueError(f"window start {s} s outside the schedule span")
    scenes = [render_window(ctx, schedule, i, s) for i, s in enumerate(starts)]
    schedule["windows"] = [{"index": i, "start_s": s, "duration_s": dur,
                            "recording": f"window_{i:02d}.sigmf-meta"} for i, s in enumerate(starts)]
    return scenes, {"schedule.json": schedule}
