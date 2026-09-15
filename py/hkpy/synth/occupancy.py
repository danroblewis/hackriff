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


# ---------------------------------------------------------------------------------------------
# ``occupancy_markov_scene`` (T-117, AWARE-042/AWARE-044/PROP-023): channels with a known two-state
# Markov on/off process (configured true FCO), hour-of-week activity modulation, a novelty emitter
# injected at a fixed hour, a periodic launch-like event, and a persistent "boring" wideband
# channel -- for M2's occupancy engine (T-118), baseline/novelty (T-119) and acceptance (T-124).
#
# **Representation.** Same on-demand-IQ idea as :func:`occupancy_multi_hour`: the ground truth is a
# seeded schedule of on/off **intervals** per channel over the whole (fast-forwardable) simulated
# span, computed once by exact renewal-process sampling; a handful of short IQ windows are rendered
# from it on demand. Two more things the multi-hour schedule doesn't need:
#
# - an **observation schedule** (irregular revisit times over the whole span, from the same
#   ``revisit_mode="random"`` process a real scheduler would produce, or an explicit list) used to
#   compute a *sampled* FCO from the truth intervals, independent of whether that revisit's IQ was
#   ever rendered -- this is what lets a downstream test assert ITU-R SM.2256 Annex 1 style binomial
#   agreement without paying for full-resolution IQ at every revisit;
# - **hour-of-week** (168-slot, calendar-aware) occupancy stats per channel, not just hour-of-day.
#
# Two-state process: OFF dwells ~Exponential(mean_off_s), ON dwells ~Exponential(mean_on_s); the
# stationary P(on) = mean_on_s/(mean_on_s+mean_off_s) is the channel's *configured* (hidden) true
# FCO. The hour-of-week channel additionally thins the OFF->ON rate by a 168-slot activity
# multiplier (thinning/rejection, as in :func:`build_schedule` above) so its realized FCO varies by
# slot; its target FCO is nominal (a rate parameter), not asserted exactly.
# ---------------------------------------------------------------------------------------------

SCENE_DEFAULTS: dict[str, Any] = {
    "start_utc": "2026-09-15T00:00:00Z",
    "span_hours": 48.0,
    "sample_rate": 500e3,
    "center_hz": 433.5e6,
    "channel_spacing_hz": 25e3,
    "channel_bandwidth_hz": 15e3,
    "markov_fcos": [0.01, 0.10, 0.50, 1.00],
    "markov_mean_on_s": 60.0,
    "power_dbfs_min": -30.0,
    "power_dbfs_max": -15.0,
    "noise_dbfs": -45.0,
    "diurnal_target_fco": 0.2,
    "diurnal_mean_on_s": 120.0,
    "diurnal_profile": [],
    "novelty_start_hour": 30.0,
    "novelty_fco": 0.4,
    "novelty_mean_on_s": 90.0,
    "event_hours_utc": [0, 12],
    "event_duration_s": 90.0,
    "boring_offset_hz": 170e3,
    "boring_bandwidth_hz": 80e3,
    "boring_power_dbfs": -20.0,
    "revisit_mode": "random",
    "revisit_mean_gap_s": 60.0,
    "revisit_min_gap_s": 5.0,
    "revisit_times_s": [],
    "revisit_ci_z": 3.0,
    "n_iq_windows": 4,
    "window_duration_s": 0.05,
    "iq_window_starts_s": [],
    "iq_windows_at_revisits": False,
    "calibration_k_db": -70.0,
}


def wilson_ci(k: int, n: int, z: float = 3.0) -> tuple[float, float]:
    """Wilson score confidence interval for a binomial proportion (SM.2256 Annex 1 style).

    ``k`` successes out of ``n`` trials; ``z=3.0`` is a generous (~99.7%) two-sided bound so the
    check is not flaky against a single seeded draw.
    """
    if n <= 0:
        return (0.0, 1.0)
    phat = k / n
    denom = 1.0 + z * z / n
    center = phat + z * z / (2 * n)
    half = z * math.sqrt(phat * (1 - phat) / n + z * z / (4 * n * n))
    return (max(0.0, (center - half) / denom), min(1.0, (center + half) / denom))


def _hour_of_week(start_dt: _dt.datetime, elapsed_s: float) -> int:
    """Calendar hour-of-week (0 = Monday 00:00 UTC .. 167) at ``start_dt + elapsed_s``."""
    t = start_dt + _dt.timedelta(seconds=elapsed_s)
    return t.weekday() * 24 + t.hour


def _week_slot_edges(start_dt: _dt.datetime, span_s: float) -> list[float]:
    """Elapsed-second boundaries of every calendar-hour slot covering ``[0, span_s)``."""
    edges = [0.0]
    boundary = start_dt.replace(minute=0, second=0, microsecond=0)
    if boundary < start_dt:
        boundary += _dt.timedelta(hours=1)
    end_dt = start_dt + _dt.timedelta(seconds=span_s)
    while boundary < end_dt:
        edges.append((boundary - start_dt).total_seconds())
        boundary += _dt.timedelta(hours=1)
    edges.append(span_s)
    return sorted(set(edges))


def _event_times(start_dt: _dt.datetime, span_s: float, event_hours_utc: list[int]) -> list[float]:
    """Elapsed seconds of every UTC clock hour in ``event_hours_utc`` within ``[0, span_s)``."""
    times = []
    boundary = start_dt.replace(minute=0, second=0, microsecond=0)
    if boundary < start_dt:
        boundary += _dt.timedelta(hours=1)
    end_dt = start_dt + _dt.timedelta(seconds=span_s)
    while boundary < end_dt:
        if boundary.hour in event_hours_utc:
            times.append((boundary - start_dt).total_seconds())
        boundary += _dt.timedelta(hours=1)
    return times


def _simulate_two_state(rng: np.random.Generator, span_s: float, mean_on_s: float,
                        mean_off_s: float) -> list[tuple[float, float]]:
    """Alternating-renewal on/off intervals; ``mean_off_s <= 0`` means always on."""
    if span_s <= 0:
        return []
    if mean_off_s <= 0:
        return [(0.0, span_s)]
    p_on = mean_on_s / (mean_on_s + mean_off_s)
    intervals: list[tuple[float, float]] = []
    t = 0.0
    if rng.uniform() < p_on:  # start already on (exponential is memoryless: draw the remainder)
        dur = min(float(rng.exponential(mean_on_s)), span_s)
        if dur > 0:
            intervals.append((0.0, dur))
        t = dur
    while t < span_s:
        t += float(rng.exponential(mean_off_s))
        if t >= span_s:
            break
        dur = min(float(rng.exponential(mean_on_s)), span_s - t)
        if dur > 0:
            intervals.append((t, dur))
        t += dur
    return intervals


def _simulate_modulated(rng: np.random.Generator, span_s: float, mean_on_s: float,
                        base_mean_off_s: float, week_profile: np.ndarray,
                        start_dt: _dt.datetime) -> list[tuple[float, float]]:
    """Like :func:`_simulate_two_state` but the off->on rate is thinned by ``week_profile[hour_of_week]``."""
    if span_s <= 0:
        return []
    lam_max = (1.0 / base_mean_off_s) * float(week_profile.max())
    intervals: list[tuple[float, float]] = []
    t = 0.0
    how0 = _hour_of_week(start_dt, 0.0)
    p_on0 = mean_on_s / (mean_on_s + base_mean_off_s / week_profile[how0])
    if rng.uniform() < p_on0:
        dur = min(float(rng.exponential(mean_on_s)), span_s)
        if dur > 0:
            intervals.append((0.0, dur))
        t = dur
    while t < span_s:
        while True:
            t += float(rng.exponential(1.0 / lam_max))
            if t >= span_s:
                break
            if rng.uniform() < week_profile[_hour_of_week(start_dt, t)] / week_profile.max():
                break
        if t >= span_s:
            break
        dur = min(float(rng.exponential(mean_on_s)), span_s - t)
        if dur > 0:
            intervals.append((t, dur))
        t += dur
    return intervals


def _sample_revisits(seed: int, span_s: float, p: dict[str, Any]) -> np.ndarray:
    """The scheduler's revisit times: ``"given"`` (explicit list) or ``"random"`` (irregular gaps)."""
    mode = p["revisit_mode"]
    if mode == "given":
        times = np.asarray(sorted(float(t) for t in p["revisit_times_s"]), dtype=np.float64)
        if times.size and (times.min() < 0 or times.max() >= span_s):
            raise ValueError("revisit_times_s outside the scene span")
        return times
    if mode != "random":
        raise ValueError(f"revisit_mode must be 'random' or 'given', got {mode!r}")
    r = rng_for(seed, "occscene", "revisits")
    mean_gap, min_gap = float(p["revisit_mean_gap_s"]), float(p["revisit_min_gap_s"])
    times: list[float] = []
    t = 0.0
    while True:
        t += max(min_gap, float(r.exponential(mean_gap)))
        if t >= span_s:
            break
        times.append(t)
    return np.asarray(times, dtype=np.float64)


def _sampled_fco(intervals: list[dict[str, Any]], channels: list[dict[str, Any]],
                 times_s: np.ndarray, z: float) -> dict[int, dict[str, Any]]:
    """FCO as a real scheduler would measure it: fraction of ``times_s`` landing inside an interval."""
    out: dict[int, dict[str, Any]] = {}
    for c in channels:
        idx = c["channel"]
        mine = sorted((iv for iv in intervals if iv["channel"] == idx), key=lambda x: x["start_s"])
        n = int(times_s.size)
        if n == 0:
            out[idx] = {"n_revisits": 0, "n_occupied": 0, "fco": None, "ci_wilson": None}
            continue
        starts = np.array([m["start_s"] for m in mine], dtype=np.float64)
        occ = np.zeros(n, dtype=bool)
        if starts.size:
            ends = starts + np.array([m["duration_s"] for m in mine], dtype=np.float64)
            pos = np.searchsorted(starts, times_s, side="right") - 1
            valid = pos >= 0
            occ[valid] = times_s[valid] < ends[pos[valid]]
        n_occ = int(occ.sum())
        out[idx] = {"n_revisits": n, "n_occupied": n_occ, "fco": n_occ / n,
                    "ci_wilson": list(wilson_ci(n_occ, n, z))}
    return out


def week_occupancy_stats(intervals: list[dict[str, Any]], channels: list[dict[str, Any]],
                         span_s: float, start_utc: str) -> dict[str, Any]:
    """Exact (interval-arithmetic) per-channel occupancy overall and per calendar hour-of-week slot."""
    start_dt = parse_utc(start_utc)
    edges = _week_slot_edges(start_dt, span_s)
    per_channel = []
    for c in channels:
        idx = c["channel"]
        mine = [iv for iv in intervals if iv["channel"] == idx]
        on = sum(iv["duration_s"] for iv in mine)
        week_slots: dict[int, dict[str, float]] = {}
        for e0, e1 in zip(edges[:-1], edges[1:], strict=True):
            slot = week_slots.setdefault(_hour_of_week(start_dt, e0), {"exposure_s": 0.0, "on_s": 0.0})
            slot["exposure_s"] += e1 - e0
            slot["on_s"] += sum(_overlap(iv["start_s"], iv["start_s"] + iv["duration_s"], e0, e1) for iv in mine)
        per_channel.append({
            "channel": idx, "kind": c["kind"], "n_intervals": len(mine), "on_time_s": on,
            "fco_realized": on / span_s if span_s > 0 else 0.0, "target_fco": c.get("target_fco"),
            "hour_of_week": {str(h): {**v, "occupancy": (v["on_s"] / v["exposure_s"]) if v["exposure_s"] else None}
                            for h, v in sorted(week_slots.items())},
        })
    return {"span_s": span_s, "n_intervals": len(intervals), "per_channel": per_channel}


def build_scene_schedule(seed: int, p: dict[str, Any]) -> dict[str, Any]:
    start_dt = parse_utc(p["start_utc"])
    span = float(p["span_hours"]) * 3600.0
    fs = float(p["sample_rate"])
    fc = float(p["center_hz"])
    spacing = float(p["channel_spacing_hz"])
    bw = float(p["channel_bandwidth_hz"])
    ch_rng = rng_for(seed, "occscene", "channels")

    channels: list[dict[str, Any]] = []
    intervals: list[dict[str, Any]] = []

    def add_channel(idx: int, kind: str, offset_hz: float, power: float, target_fco: float | None,
                    mean_on_s: float | None, mean_off_s: float | None, value: str) -> None:
        channels.append({"channel": idx, "kind": kind, "center_hz": fc + offset_hz,
                         "offset_hz": offset_hz, "bandwidth_hz": bw, "power_dbfs": power,
                         "target_fco": target_fco, "mean_on_s": mean_on_s, "mean_off_s": mean_off_s,
                         "identity": {"type": "channel-user", "value": value}})

    def add_intervals(idx: int, ivs: list[tuple[float, float]]) -> None:
        for s, d in ivs:
            d = min(d, span - s)
            if d > 0:
                intervals.append({"channel": idx, "start_s": round(s, 6), "duration_s": round(d, 6)})

    # Markov channels: one per configured target FCO --------------------------------------------
    fcos = [float(x) for x in p["markov_fcos"]]
    n_markov = len(fcos)
    first_offset = -(n_markov + 2) * spacing
    mean_on_s = float(p["markov_mean_on_s"])
    for i, fco in enumerate(fcos):
        power = float(ch_rng.uniform(p["power_dbfs_min"], p["power_dbfs_max"]))
        mean_off_s = 0.0 if fco >= 1.0 else mean_on_s * (1.0 - fco) / fco
        ivs = _simulate_two_state(rng_for(seed, "occscene", "markov", i), span, mean_on_s, mean_off_s)
        add_channel(i, "markov", first_offset + i * spacing, power, fco, mean_on_s, mean_off_s, f"markov{i}")
        add_intervals(i, ivs)

    # Hour-of-week-modulated channel --------------------------------------------------------------
    diurnal_idx = n_markov
    profile24 = np.asarray(p["diurnal_profile"] or DIURNAL_PROFILE, dtype=np.float64)
    if profile24.shape != (24,) or np.any(profile24 < 0) or profile24.sum() <= 0:
        raise ValueError("diurnal_profile needs 24 non-negative values")
    profile24 = profile24 / profile24.mean()
    week_profile = np.tile(profile24, 7)
    d_fco, d_mean_on = float(p["diurnal_target_fco"]), float(p["diurnal_mean_on_s"])
    d_mean_off = d_mean_on * (1.0 - d_fco) / d_fco if d_fco < 1.0 else 0.0
    d_power = float(ch_rng.uniform(p["power_dbfs_min"], p["power_dbfs_max"]))
    d_ivs = ([(0.0, span)] if d_mean_off <= 0 else
             _simulate_modulated(rng_for(seed, "occscene", "diurnal"), span, d_mean_on, d_mean_off,
                                 week_profile, start_dt))
    add_channel(diurnal_idx, "diurnal", first_offset + n_markov * spacing, d_power, d_fco, d_mean_on,
               d_mean_off, "diurnal0")
    add_intervals(diurnal_idx, d_ivs)

    # Novelty: silent until novelty_start_hour, then Markov on/off ---------------------------------
    novelty_idx = diurnal_idx + 1
    novelty_start_s = float(p["novelty_start_hour"]) * 3600.0
    n_fco, n_mean_on = float(p["novelty_fco"]), float(p["novelty_mean_on_s"])
    n_mean_off = 0.0 if n_fco >= 1.0 else n_mean_on * (1.0 - n_fco) / n_fco
    n_power = float(ch_rng.uniform(p["power_dbfs_min"], p["power_dbfs_max"]))
    if novelty_start_s < span:
        sub = _simulate_two_state(rng_for(seed, "occscene", "novelty"), span - novelty_start_s,
                                  n_mean_on, n_mean_off)
        n_ivs = [(novelty_start_s + s, d) for s, d in sub]
    else:
        n_ivs = []
    n_offset = first_offset + (n_markov + 1) * spacing
    add_channel(novelty_idx, "novelty", n_offset, n_power, n_fco, n_mean_on, n_mean_off, "novelty0")
    add_intervals(novelty_idx, n_ivs)

    # Periodic launch-like event: fixed UTC clock hours (default 00Z/12Z) --------------------------
    event_idx = novelty_idx + 1
    event_hours = [int(h) for h in p["event_hours_utc"]]
    event_dur = float(p["event_duration_s"])
    event_times = _event_times(start_dt, span, event_hours)
    e_offset = first_offset + (n_markov + 2) * spacing
    e_power = float(ch_rng.uniform(p["power_dbfs_min"], p["power_dbfs_max"]))
    add_channel(event_idx, "event", e_offset, e_power, None, event_dur, None, "launch-event")
    add_intervals(event_idx, [(t, event_dur) for t in event_times])

    # Boring band: persistent, always on, wideband -------------------------------------------------
    boring_idx = event_idx + 1
    boring_offset, boring_bw = float(p["boring_offset_hz"]), float(p["boring_bandwidth_hz"])
    channels.append({"channel": boring_idx, "kind": "boring", "center_hz": fc + boring_offset,
                     "offset_hz": boring_offset, "bandwidth_hz": boring_bw,
                     "power_dbfs": float(p["boring_power_dbfs"]), "target_fco": 1.0,
                     "mean_on_s": None, "mean_off_s": None,
                     "identity": {"type": "channel-user", "value": "boring-band"}})
    intervals.append({"channel": boring_idx, "start_s": 0.0, "duration_s": span})

    for c in channels:
        if abs(c["offset_hz"]) + c["bandwidth_hz"] / 2 > 0.45 * fs:
            raise ValueError(f"channel {c['channel']} ({c['kind']}) at offset {c['offset_hz']} Hz "
                             f"is outside the rendered bandwidth")

    intervals.sort(key=lambda iv: (iv["start_s"], iv["channel"]))
    for i, iv in enumerate(intervals):
        iv["index"] = i

    z = float(p["revisit_ci_z"])
    obs_times = _sample_revisits(seed, span, p)
    return {
        "kind": "occupancy-markov-scene",
        "start_utc": p["start_utc"],
        "span_s": span,
        "channels": channels,
        "intervals": intervals,
        "novelty": {"channel": novelty_idx, "start_hour": float(p["novelty_start_hour"]),
                   "start_s": novelty_start_s, "center_hz": fc + n_offset, "target_fco": n_fco},
        "event_schedule": [{"start_s": t, "duration_s": min(event_dur, span - t),
                            "center_hz": fc + e_offset} for t in event_times],
        "observation_schedule": {"mode": p["revisit_mode"], "n_revisits": int(obs_times.size),
                                 "times_s": obs_times.tolist()},
        "stats": week_occupancy_stats(intervals, channels, span, p["start_utc"]),
        "sampled_fco": {"z": z, "by_channel": _sampled_fco(intervals, channels, obs_times, z)},
    }


def render_scene_window(ctx: Ctx, schedule: dict[str, Any], index: int, start_s: float) -> Scene:
    """IQ for ``[start_s, start_s + window_duration_s)`` of the scene, with truth annotations."""
    p = ctx.params
    fs = float(p["sample_rate"])
    dur = float(p["window_duration_s"])
    n = int(round(dur * fs))
    g0 = int(round(start_s * fs))
    scene = ctx.scene(f"window_{index:02d}", fs, n,
                      f"hkpy.synth occupancy_markov_scene window {index} at t={start_s:.6f} s")
    fc = float(p["center_hz"])
    cap = scene.add_capture(0, n, fc, utc_plus(schedule["start_utc"], g0 / fs),
                            calibration_k_db=float(p["calibration_k_db"]))
    cap.floor_dbfs_per_hz = float(p["noise_dbfs"]) - db(fs)
    scene.add_samples(0, complex_noise(rng_for(ctx.seed, "occscene", "noise", g0), n, float(p["noise_dbfs"])))
    scene.add_floor(0, n, cap.floor_dbfs_per_hz)
    w0, w1 = g0 / fs, (g0 + n) / fs
    chans = {c["channel"]: c for c in schedule["channels"]}
    in_window = []
    for iv in schedule["intervals"]:
        b0, b1 = iv["start_s"], iv["start_s"] + iv["duration_s"]
        if b1 <= w0 or b0 >= w1:
            continue
        ch = chans[iv["channel"]]
        off = ch["center_hz"] - fc
        i0 = max(0, int(math.ceil(b0 * fs)) - g0)
        i1 = min(n, int(math.ceil(b1 * fs)) - g0)
        if i1 <= i0:
            continue
        r = rng_for(ctx.seed, "occscene", "interval", iv["index"])
        phase0 = float(r.uniform(0, 2 * math.pi))
        t_abs = (g0 + np.arange(i0, i1)) / fs
        amp = math.sqrt(undb(ch["power_dbfs"]))
        scene.add_samples(i0, amp * np.exp(1j * (phase0 + 2 * math.pi * off * t_abs)))
        hbw = ch["bandwidth_hz"] / 2
        scene.annotate(i0, i1 - i0, ch["center_hz"] - hbw, ch["center_hz"] + hbw, f"occupancy-{ch['kind']}",
                       scene.emission_truth(
                           cap, off, ch["bandwidth_hz"], ch["power_dbfs"], kind=f"occupancy-{ch['kind']}",
                           modulation="cw", channel=iv["channel"], interval_index=iv["index"],
                           interval_start_s=b0, interval_duration_s=iv["duration_s"],
                           clipped_by_window=bool(b0 < w0 or b1 > w1), target_fco=ch.get("target_fco"),
                           identity=ch["identity"]))
        in_window.append(iv["index"])
    scene.scenario_truth["window"] = {"index": index, "start_s": w0, "duration_s": n / fs,
                                      "start_utc": cap.datetime, "intervals": in_window,
                                      "schedule_file": "schedule.json"}
    return scene


def _choose_iq_windows(schedule: dict[str, Any], p: dict[str, Any]) -> list[float]:
    """A handful of windows: right after the novelty injection, at the first event, then spread out."""
    span, dur = schedule["span_s"], float(p["window_duration_s"])

    def clip(s: float) -> float:
        return min(max(0.0, s), max(0.0, span - dur))

    forced = [clip(schedule["novelty"]["start_s"] + 5.0)]
    if schedule["event_schedule"]:
        forced.append(clip(schedule["event_schedule"][0]["start_s"]))
    remaining = max(0, int(p["n_iq_windows"]) - len(forced))
    if remaining > 0:
        step = span / (remaining + 1)
        forced += [clip(step * (i + 1)) for i in range(remaining)]
    seen: set[float] = set()
    out = []
    for s in forced:
        key = round(s, 6)
        if key not in seen:
            seen.add(key)
            out.append(s)
    return out


def occupancy_markov_scene(ctx: Ctx) -> tuple[list[Scene], dict[str, Any]]:
    p = ctx.params
    dur = float(p["window_duration_s"])
    schedule = build_scene_schedule(ctx.seed, p)
    span = schedule["span_s"]
    if dur <= 0 or dur > span:
        raise ValueError("window_duration_s must be positive and within span_hours")
    if p["iq_windows_at_revisits"]:
        # One window per observation-schedule revisit (T-125: the mock SDR serves them as a
        # time-compressed scene, jumping stream time between revisits).
        starts = [min(t, span - dur) for t in schedule["observation_schedule"]["times_s"]]
    elif p["iq_window_starts_s"]:
        starts = [float(s) for s in p["iq_window_starts_s"]]
    else:
        starts = _choose_iq_windows(schedule, p)
    for s in starts:
        if not 0 <= s <= span - dur:
            raise ValueError(f"window start {s} s outside the scene span")
    scenes = [render_scene_window(ctx, schedule, i, s) for i, s in enumerate(starts)]
    schedule["windows"] = [{"index": i, "start_s": s, "duration_s": dur,
                            "recording": f"window_{i:02d}.sigmf-meta"} for i, s in enumerate(starts)]
    return scenes, {"schedule.json": schedule}
