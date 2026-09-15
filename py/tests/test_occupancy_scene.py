"""``occupancy_markov_scene`` (T-117, AWARE-042/AWARE-044/PROP-023): Markov on/off channels with
known true FCO, hour-of-week patterns, injected novelty, a periodic launch-like event, and a
boring persistent band -- ground truth for M2's occupancy engine (T-118) and baseline/novelty
(T-119). These tests check the *generator's* truth against its own configuration (the hidden
truth is never adjusted to make a test pass); T-118/T-119/T-124 check the pipeline's measurement
against this same hidden truth.
"""

from __future__ import annotations

import json
import math
import time
from pathlib import Path

import pytest

from hkpy.synth import SCENARIOS, generate, resolve_params
from hkpy.synth import occupancy as occ

SEED = 1


def schedule(seed: int = SEED, **overrides):
    p = resolve_params("occupancy_markov_scene", overrides)
    return occ.build_scene_schedule(seed, p), p


def by_channel(sched, idx):
    [c] = [c for c in sched["stats"]["per_channel"] if c["channel"] == idx]
    return c


# ---- Markov channels: configured FCO vs. what actually happened -----------------------------


def _time_average_tolerance(target_fco: float, mean_on_s: float | None, mean_off_s: float | None,
                            span_s: float, z: float = 5.0) -> float:
    """A generous bound on |realized time-average FCO - target| for an alternating-renewal process:
    treats each on/off cycle as one approximately-independent draw (z=5 is far past any plausible
    single-seed fluctuation, so this is not a flaky statistical test -- generation is deterministic)."""
    if not mean_off_s or mean_off_s <= 0:
        return 1e-9
    n_cycles = span_s / (mean_on_s + mean_off_s)
    return max(0.005, z * math.sqrt(target_fco * (1 - target_fco) / max(n_cycles, 1.0)))


def test_markov_channel_realized_fco_matches_configured():
    sched, _ = schedule()
    for c in sched["channels"]:
        if c["kind"] != "markov":
            continue
        stat = by_channel(sched, c["channel"])
        tol = _time_average_tolerance(c["target_fco"], c["mean_on_s"], c["mean_off_s"], sched["span_s"])
        assert abs(stat["fco_realized"] - c["target_fco"]) <= tol, (c, stat, tol)


def test_sampled_revisit_fco_agrees_with_configured_within_binomial_ci():
    """SM.2256 Annex 1 style check: a scheduler that only ever sees the channel at irregular
    revisit times should still estimate the configured FCO, within a Wilson binomial CI."""
    sched, _ = schedule()
    by_ch = sched["sampled_fco"]["by_channel"]
    for c in sched["channels"]:
        if c["kind"] != "markov":
            continue
        s = by_ch[str(c["channel"])] if str(c["channel"]) in by_ch else by_ch[c["channel"]]
        assert s["n_revisits"] > 100, "too few revisits for a meaningful confidence interval"
        lo, hi = s["ci_wilson"]
        assert lo <= c["target_fco"] <= hi, (c["channel"], c["target_fco"], s)


def test_markov_fcos_are_distinguishable():
    """The four configured points (1/10/50/100%) don't collapse into each other."""
    sched, _ = schedule()
    realized = sorted(by_channel(sched, c["channel"])["fco_realized"]
                      for c in sched["channels"] if c["kind"] == "markov")
    assert realized == sorted(realized)
    assert realized[0] < 0.05 < realized[1] < realized[2] < 0.95 < realized[-1] <= 1.0 + 1e-9


# ---- Novelty injection at hour 30 -------------------------------------------------------------


def test_novelty_injected_at_hour_30():
    sched, p = schedule()
    assert p["novelty_start_hour"] == 30.0
    nov = sched["novelty"]
    assert nov["start_hour"] == 30.0
    assert nov["start_s"] == pytest.approx(30.0 * 3600.0)
    mine = [iv for iv in sched["intervals"] if iv["channel"] == nov["channel"]]
    assert mine, "the novelty channel should be active at least once in a 48 h scene"
    assert all(iv["start_s"] >= nov["start_s"] for iv in mine)
    # nothing before hour 30 at all: no interval overlaps [0, start_s)
    assert not any(iv["start_s"] < nov["start_s"] for iv in sched["intervals"]
                  if iv["channel"] == nov["channel"])
    # realized FCO measured over the channel's *active* lifetime (not the whole 48 h) should be
    # near its configured value.
    active_span = sched["span_s"] - nov["start_s"]
    on_time = sum(iv["duration_s"] for iv in mine)
    tol = _time_average_tolerance(nov["target_fco"], p["novelty_mean_on_s"],
                                  p["novelty_mean_on_s"] * (1 - nov["target_fco"]) / nov["target_fco"],
                                  active_span)
    assert abs(on_time / active_span - nov["target_fco"]) <= tol


def test_novelty_start_hour_is_configurable():
    sched, _ = schedule(novelty_start_hour=10.0, span_hours=20.0)
    assert sched["novelty"]["start_s"] == pytest.approx(10.0 * 3600.0)
    nov_ch = sched["novelty"]["channel"]
    assert all(iv["start_s"] >= 10.0 * 3600.0 for iv in sched["intervals"] if iv["channel"] == nov_ch)


# ---- Periodic launch-like event (00Z/12Z) ------------------------------------------------------


def test_periodic_event_lands_on_00z_and_12z():
    sched, p = schedule()  # default start_utc = 2026-09-15T00:00:00Z
    events = sched["event_schedule"]
    assert events, "expected at least one launch-like event in a 48 h scene"
    for e in events:
        assert e["start_s"] % (12 * 3600.0) == 0.0
        assert e["duration_s"] == pytest.approx(p["event_duration_s"])
    # every 12 h boundary in [0, span) is an event (start_utc is exactly on an hour boundary)
    expected = int(sched["span_s"] // (12 * 3600.0))
    assert len(events) == expected


def test_event_hours_are_configurable():
    sched, _ = schedule(event_hours_utc=[6], span_hours=30.0)
    events = sched["event_schedule"]
    for e in events:
        assert (e["start_s"] / 3600.0) % 24.0 == 6.0


# ---- Boring band ---------------------------------------------------------------------------


def test_boring_band_is_persistently_on():
    sched, _ = schedule()
    boring = [c for c in sched["channels"] if c["kind"] == "boring"][0]
    stat = by_channel(sched, boring["channel"])
    assert stat["fco_realized"] == 1.0
    assert stat["n_intervals"] == 1
    mine = [iv for iv in sched["intervals"] if iv["channel"] == boring["channel"]][0]
    assert mine["start_s"] == 0.0
    assert mine["duration_s"] == pytest.approx(sched["span_s"])
    assert boring["bandwidth_hz"] > [c for c in sched["channels"] if c["kind"] == "markov"][0]["bandwidth_hz"]


# ---- Hour-of-week structure ---------------------------------------------------------------------


def test_hour_of_week_slots_cover_the_full_week_range():
    sched, _ = schedule()
    for c in sched["stats"]["per_channel"]:
        slots = c["hour_of_week"]
        assert slots, c
        for k in slots:
            assert 0 <= int(k) <= 167


def test_hour_of_week_modulation_shapes_the_diurnal_channel():
    """The diurnal channel's per-slot occupancy should track its activity multiplier: busiest
    daytime hours (from DIURNAL_PROFILE) should show more occupancy than the quietest night hour,
    aggregated over the two days in a 48 h scene so a single slot's noise averages out less."""
    sched, _ = schedule()
    diurnal = [c for c in sched["channels"] if c["kind"] == "diurnal"][0]
    stat = by_channel(sched, diurnal["channel"])
    busiest_hour = int(max(range(24), key=lambda h: occ.DIURNAL_PROFILE[h]))
    quietest_hour = int(min(range(24), key=lambda h: occ.DIURNAL_PROFILE[h]))
    busy = [v["occupancy"] for h, v in stat["hour_of_week"].items() if int(h) % 24 == busiest_hour
           and v["occupancy"] is not None]
    quiet = [v["occupancy"] for h, v in stat["hour_of_week"].items() if int(h) % 24 == quietest_hour
            and v["occupancy"] is not None]
    assert busy and quiet
    assert sum(busy) / len(busy) > sum(quiet) / len(quiet)


# ---- Observation schedule / irregular revisit ---------------------------------------------------


def test_observation_schedule_is_irregular_and_covers_the_span():
    sched, _ = schedule()
    times = sched["observation_schedule"]["times_s"]
    assert sched["observation_schedule"]["n_revisits"] == len(times)
    assert len(times) > 500
    assert times == sorted(times)
    assert times[0] >= 0.0 and times[-1] < sched["span_s"]
    gaps = [b - a for a, b in zip(times, times[1:])]
    assert len(set(round(g, 3) for g in gaps)) > 10, "revisit gaps should be irregular, not fixed"


def test_observation_schedule_accepts_an_explicit_revisit_list():
    given = [0.0, 3600.0, 7200.0, 100000.0]
    sched, _ = schedule(revisit_mode="given", revisit_times_s=given)
    assert sched["observation_schedule"]["times_s"] == given
    assert sched["observation_schedule"]["mode"] == "given"


# ---- Determinism -----------------------------------------------------------------------------


def test_schedule_generation_is_deterministic():
    a, _ = schedule(seed=7)
    b, _ = schedule(seed=7)
    c, _ = schedule(seed=8)
    assert json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)
    assert json.dumps(a, sort_keys=True) != json.dumps(c, sort_keys=True)


def test_full_generation_is_deterministic(tmp_path):
    small = {"span_hours": 2.0, "novelty_start_hour": 1.0, "n_iq_windows": 1, "window_duration_s": 0.02}
    a = generate("occupancy_markov_scene", 3, tmp_path / "a", small)
    b = generate("occupancy_markov_scene", 3, tmp_path / "b", small)
    assert json.loads((a.parent / "schedule.json").read_text()) == json.loads((b.parent / "schedule.json").read_text())
    ra = (tmp_path / "a" / "window_00.sigmf-data").read_bytes()
    rb = (tmp_path / "b" / "window_00.sigmf-data").read_bytes()
    assert ra == rb


def test_iq_windows_can_follow_the_observation_schedule(tmp_path):
    # T-125: one IQ window per revisit, for the mock SDR's time-compressed scene replay.
    dur = 0.02
    small = {"span_hours": 2.0, "novelty_start_hour": 1.0, "revisit_mean_gap_s": 900.0,
             "window_duration_s": dur, "iq_windows_at_revisits": True}
    manifest = generate("occupancy_markov_scene", 3, tmp_path / "rev", small)
    sched = json.loads((manifest.parent / "schedule.json").read_text())
    times = sched["observation_schedule"]["times_s"]
    starts = [w["start_s"] for w in sched["windows"]]
    assert len(times) > 1
    assert starts == [min(t, sched["span_s"] - dur) for t in times]
    assert len(json.loads(manifest.read_text())["recordings"]) == len(times)


# ---- Size / timing (report only; generous CI bounds) -------------------------------------------


def test_default_scene_is_small_and_fast_enough_for_ci(tmp_path):
    t0 = time.monotonic()
    manifest = generate("occupancy_markov_scene", SEED, tmp_path / "full", {})
    elapsed = time.monotonic() - t0
    total_bytes = sum(f.stat().st_size for f in manifest.parent.iterdir() if f.is_file())
    man = json.loads(manifest.read_text())
    print(f"\noccupancy_markov_scene (default, 48 h span): generated in {elapsed:.3f}s, "
         f"{total_bytes / 1024:.1f} KiB across {len(man['recordings'])} recordings + schedule.json")
    assert elapsed < 5.0, f"generation took {elapsed:.2f}s, expected well under 5s for CI"
    assert total_bytes < 5 * 1024 * 1024, f"{total_bytes} bytes on disk, expected well under 5 MiB"


def test_scenario_is_registered():
    assert "occupancy_markov_scene" in SCENARIOS
    assert SCENARIOS["occupancy_markov_scene"].use_cases == ("AWARE-042", "AWARE-044", "PROP-023")
