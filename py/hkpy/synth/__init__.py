"""Synthetic IQ generator with ground truth (T-023). Tooling only, never the real-time path.

``generate(scenario, seed, out_dir, params, datatype)`` is deterministic in (scenario, seed, params,
datatype, generator version). It writes one or more SigMF recordings, any extra truth files, and a
``manifest.json`` into ``out_dir``. CLI: ``python -m hkpy.synth <scenario> --seed N --out DIR
[--param k=v ...] [--datatype ci8|cf32_le]``; ``--list`` shows scenarios and their parameters.

Scenarios (use cases): ``tone`` (building block), ``fsk_burst_train`` (AWARE-036),
``noise_floor_rise`` (AWARE-006), ``injected_floor`` (SPACE-050), ``occupancy_multi_hour``
(AWARE-042), ``occupancy_markov_scene`` (AWARE-042/AWARE-044/PROP-023, T-117), ``fm_broadcast_rds``
(SIGNAL-062), ``adsb_squitter`` (SIGNAL-001), ``pocsag_pagers`` (SIGNAL-062, M1 tutorial fixture
T-098), ``acars_message`` (SIGNAL-062, M1 tutorial fixture T-098, synthetic-only -- see
:mod:`hkpy.synth.acars`), ``trunk_control_channel`` (C23, T-267), ``lora_ism_burst``
(SIGNAL-062/AWARE-053, T-255: chirps with no stable frequency in 902-928 MHz US ISM -- see
:mod:`hkpy.synth.lora_scene`). Every scenario also accepts the impairment parameters in
:data:`hkpy.synth.impairments.IMPAIRMENT_DEFAULTS`.

Truth conventions (dBFS reference, calibration constant, annotation roles) are documented in
:mod:`hkpy.synth.scene`; the ``hackriff:truth`` fields per kind are listed in ``py/README.md``.
"""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from hkpy.synth import impairments, lora_scene, occupancy, scenarios, trunk_scene
from hkpy.synth.scene import GENERATOR, GENERATOR_VERSION, SUPPORTED_DATATYPES, _jsonable


@dataclass(frozen=True)
class ScenarioSpec:
    fn: Callable[[scenarios.Ctx], tuple[list[Any], dict[str, Any]]]
    defaults: dict[str, Any]
    use_cases: tuple[str, ...]
    summary: str


SCENARIOS: dict[str, ScenarioSpec] = {
    "tone": ScenarioSpec(scenarios.tone, scenarios.TONE_DEFAULTS, (), "CW tone in white noise"),
    "fsk_burst_train": ScenarioSpec(scenarios.fsk_burst_train, scenarios.FSK_DEFAULTS, ("AWARE-036",),
                                    "periodic 2-FSK sensor bursts: preamble, sync, payload, CRC-16"),
    "noise_floor_rise": ScenarioSpec(scenarios.noise_floor_rise, scenarios.FLOOR_RISE_DEFAULTS,
                                     ("AWARE-006",), "GNSS L1 band with a known floor step at t0"),
    "injected_floor": ScenarioSpec(scenarios.injected_floor, scenarios.INJECTED_FLOOR_DEFAULTS,
                                   ("SPACE-050",), "known calibrated floors across band segments"),
    "occupancy_multi_hour": ScenarioSpec(occupancy.occupancy_multi_hour, occupancy.OCCUPANCY_DEFAULTS,
                                         ("AWARE-042",), "multi-hour burst schedule + on-demand IQ windows"),
    "occupancy_markov_scene": ScenarioSpec(
        occupancy.occupancy_markov_scene, occupancy.SCENE_DEFAULTS,
        ("AWARE-042", "AWARE-044", "PROP-023"),
        "Markov on/off channels (known FCO), hour-of-week pattern, injected novelty, periodic "
        "event, boring band, irregular revisit schedule"),
    "fm_broadcast_rds": ScenarioSpec(scenarios.fm_broadcast_rds, scenarios.FM_DEFAULTS, ("SIGNAL-062",),
                                     "stereo WFM with pilot and RDS 0A (PI, PS)"),
    "adsb_squitter": ScenarioSpec(scenarios.adsb_squitter, scenarios.ADSB_DEFAULTS, ("SIGNAL-001",),
                                  "Mode S DF17 squitters with valid CRC-24"),
    "pocsag_pagers": ScenarioSpec(scenarios.pocsag_pagers, scenarios.POCSAG_DEFAULTS, ("SIGNAL-062",),
                                  "multi-channel 2-FSK POCSAG (512/1200/2400 Bd), BCH(31,21)+parity"),
    "acars_message": ScenarioSpec(scenarios.acars_message, scenarios.ACARS_DEFAULTS, ("SIGNAL-062",),
                                  "AM+MSK 2400 Bd VHF ACARS, SYN/SOH..ETX framing, CRC-16/KERMIT incl parity (acarsdec convention)"),
    "trunk_control_channel": ScenarioSpec(
        trunk_scene.trunk_control_channel, trunk_scene.TRUNK_CC_DEFAULTS, (),
        "continuous C4FM trunking control channel (frame sync + CRC) beside an unframed "
        "continuous 4FSK decoy and bursty NBFM, on the 12.5 kHz LMR raster"),
    "lora_ism_burst": ScenarioSpec(
        lora_scene.lora_ism_burst, lora_scene.LORA_DEFAULTS, ("SIGNAL-062", "AWARE-053"),
        "LoRa CSS up-chirp packets in 902-928 MHz US ISM (hidden SF/BW/CR/payload) beside a "
        "steady CW carrier and short fixed-frequency FSK bursts"),
}


class ParamError(ValueError):
    pass


def scenario_defaults(scenario: str) -> dict[str, Any]:
    spec = _spec(scenario)
    return {**impairments.IMPAIRMENT_DEFAULTS, **spec.defaults}


def resolve_params(scenario: str, overrides: Mapping[str, Any] | None = None) -> dict[str, Any]:
    """Merges overrides into the defaults. String values (from the CLI) are coerced to the default's type."""
    params = scenario_defaults(scenario)
    for key, value in (overrides or {}).items():
        if key not in params:
            raise ParamError(f"{scenario}: unknown parameter {key!r}; valid: {', '.join(sorted(params))}")
        params[key] = _coerce(key, value, params[key]) if isinstance(value, str) else value
    return params


def generate(scenario: str, seed: int, out_dir: str | Path, params: Mapping[str, Any] | None = None,
             datatype: str = "ci8") -> Path:
    """Generates a scenario into ``out_dir`` and returns the path of its ``manifest.json``."""
    if datatype not in SUPPORTED_DATATYPES:
        raise ParamError(f"datatype must be one of {SUPPORTED_DATATYPES}")
    spec = _spec(scenario)
    resolved = resolve_params(scenario, params)
    ctx = scenarios.Ctx(scenario, int(seed), resolved, datatype, list(spec.use_cases))
    scenes, files = spec.fn(ctx)
    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    recordings = []
    for scene in scenes:
        impairments.apply(scene, resolved)
        recordings.append(scene.write(out).name)
    for name, obj in files.items():
        (out / name).write_text(json.dumps(_jsonable(obj), indent=2) + "\n", encoding="utf-8")
    manifest = {
        "generator": GENERATOR,
        "generator_version": GENERATOR_VERSION,
        "scenario": scenario,
        "seed": int(seed),
        "datatype": datatype,
        "use_cases": list(spec.use_cases),
        "params": resolved,
        "recordings": recordings,
        "files": sorted(files),
    }
    path = out / "manifest.json"
    path.write_text(json.dumps(_jsonable(manifest), indent=2) + "\n", encoding="utf-8")
    return path


def _spec(scenario: str) -> ScenarioSpec:
    try:
        return SCENARIOS[scenario]
    except KeyError:
        raise ParamError(f"unknown scenario {scenario!r}; valid: {', '.join(SCENARIOS)}") from None


def _scalar(raw: str, like: Any) -> Any:
    raw = raw.strip()
    if raw.lower() in ("none", "null"):
        return None
    if isinstance(like, bool):
        if raw.lower() in ("1", "true", "yes", "on"):
            return True
        if raw.lower() in ("0", "false", "no", "off"):
            return False
        raise ValueError(f"not a boolean: {raw!r}")
    if isinstance(like, int):
        try:
            return int(raw, 0)
        except ValueError:
            f = float(raw)
            if not f.is_integer():
                raise
            return int(f)
    if isinstance(like, float):
        return float(raw)
    if isinstance(like, str):
        return raw
    try:
        return float(raw)
    except ValueError:
        return raw


def _coerce(key: str, raw: str, default: Any) -> Any:
    try:
        if isinstance(default, list):
            like = default[0] if default else None
            return [_scalar(item, like) for item in raw.split(",") if item.strip()]
        return _scalar(raw, default)
    except ValueError as exc:
        raise ParamError(f"parameter {key}: {exc}") from None


__all__ = ["SCENARIOS", "ParamError", "generate", "resolve_params", "scenario_defaults"]
