"""Minimal SigMF ``.sigmf-meta`` reader/writer, consistent with ``hk_model::sigmf`` (Rust).

Documents are plain dicts using SigMF key names. Unknown keys are preserved on read and write.
The hackriff extension (``docs/sigmf-extension.md``) adds:

- ``hackriff:provenance`` on ``global`` or a capture. It uses the Rust ``Provenance`` field names
  and enum spellings; build it with :func:`provenance`.
- ``hackriff:truth`` on an annotation: a free-form ground-truth object.

Metadata only. Sample I/O belongs to the synthetic generator and replay tooling.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path
from typing import Any

SIGMF_VERSION = "1.2.0"
HACKRIFF_EXTENSION = "hackriff"
HACKRIFF_EXTENSION_VERSION = "0.1.0"
PROVENANCE_KEY = "hackriff:provenance"
TRUTH_KEY = "hackriff:truth"

#: SigMF datatype -> (numpy dtype of one scalar component, is_complex). Mirrors Rust ``Datatype``.
DATATYPES: dict[str, tuple[str, bool]] = {
    "ri8": ("i1", False),
    "ru8": ("u1", False),
    "ci8": ("i1", True),
    "cu8": ("u1", True),
    "ri16_le": ("<i2", False),
    "ci16_le": ("<i2", True),
    "ru16_le": ("<u2", False),
    "cu16_le": ("<u2", True),
    "ri32_le": ("<i4", False),
    "ci32_le": ("<i4", True),
    "rf32_le": ("<f4", False),
    "cf32_le": ("<f4", True),
    "rf64_le": ("<f8", False),
    "cf64_le": ("<f8", True),
}

CLOCK_SOURCES = frozenset({"internal", "external", "gpsdo"})
TIMESTAMP_METHODS = frozenset(
    {"host-arrival", "gnss-tagged", "external-reference", "synthetic", "unknown"}
)
_TUNE_FIELDS = ("center_hz", "sample_rate_hz", "lna_db", "vga_db", "amp_on", "bandwidth_hz")
_PROVENANCE_REQUIRED = (
    "device_id",
    "tune",
    "clip_count",
    "overload",
    "clock_source",
    "clock_locked",
    "timestamp_method",
)


class SigmfError(ValueError):
    """The document violates the SigMF keys hackriff relies on."""


def bytes_per_sample(datatype: str) -> int:
    """Bytes per sample in the ``.sigmf-data`` file (both components for complex types)."""
    import numpy as np

    component, is_complex = _datatype(datatype)
    return np.dtype(component).itemsize * (2 if is_complex else 1)


def provenance(
    device_id: str,
    *,
    center_hz: float,
    sample_rate_hz: float,
    lna_db: float,
    vga_db: float,
    amp_on: bool,
    bandwidth_hz: float,
    clip_count: int = 0,
    overload: bool = False,
    clock_source: str = "internal",
    clock_locked: bool = True,
    timestamp_method: str = "host-arrival",
    temperature_c: float | None = None,
    antenna_port: str | None = None,
    calibration_state_ref: str | None = None,
    spur_mask_ref: str | None = None,
    timestamp_error_budget_ns: int | None = None,
) -> dict[str, Any]:
    """Builds a ``hackriff:provenance`` object (Rust ``hk_model::Provenance``)."""
    p: dict[str, Any] = {
        "device_id": device_id,
        "tune": {
            "center_hz": float(center_hz),
            "sample_rate_hz": float(sample_rate_hz),
            "lna_db": float(lna_db),
            "vga_db": float(vga_db),
            "amp_on": bool(amp_on),
            "bandwidth_hz": float(bandwidth_hz),
        },
        "clip_count": int(clip_count),
        "overload": bool(overload),
        "clock_source": clock_source,
        "clock_locked": bool(clock_locked),
        "timestamp_method": timestamp_method,
    }
    optional = {
        "temperature_c": temperature_c,
        "antenna_port": antenna_port,
        "calibration_state_ref": calibration_state_ref,
        "spur_mask_ref": spur_mask_ref,
        "timestamp_error_budget_ns": timestamp_error_budget_ns,
    }
    p.update({k: v for k, v in optional.items() if v is not None})
    _validate_provenance(p, "provenance")
    return p


def new_meta(
    datatype: str,
    sample_rate: float | None = None,
    *,
    description: str | None = None,
    author: str | None = None,
    license: str | None = None,
    hw: str | None = None,
    recorder: str | None = None,
    provenance: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """An empty document at SIGMF_VERSION with the hackriff extension declared."""
    _datatype(datatype)
    glob: dict[str, Any] = {"core:datatype": datatype, "core:version": SIGMF_VERSION}
    optional = {
        "core:sample_rate": None if sample_rate is None else float(sample_rate),
        "core:description": description,
        "core:author": author,
        "core:license": license,
        "core:hw": hw,
        "core:recorder": recorder,
    }
    glob.update({k: v for k, v in optional.items() if v is not None})
    glob["core:extensions"] = [
        {"name": HACKRIFF_EXTENSION, "version": HACKRIFF_EXTENSION_VERSION, "optional": True}
    ]
    if provenance is not None:
        glob[PROVENANCE_KEY] = provenance
    return {"global": glob, "captures": [], "annotations": []}


def add_capture(
    meta: dict[str, Any],
    sample_start: int,
    *,
    frequency: float | None = None,
    datetime: str | None = None,
    provenance: dict[str, Any] | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Appends a capture segment and returns it."""
    cap: dict[str, Any] = {"core:sample_start": int(sample_start)}
    if frequency is not None:
        cap["core:frequency"] = float(frequency)
    if datetime is not None:
        cap["core:datetime"] = datetime
    if provenance is not None:
        cap[PROVENANCE_KEY] = provenance
    cap.update(extra or {})
    meta["captures"].append(cap)
    return cap


def add_annotation(
    meta: dict[str, Any],
    sample_start: int,
    *,
    sample_count: int | None = None,
    freq_lower_edge: float | None = None,
    freq_upper_edge: float | None = None,
    label: str | None = None,
    comment: str | None = None,
    truth: dict[str, Any] | None = None,
    extra: dict[str, Any] | None = None,
) -> dict[str, Any]:
    """Appends an annotation and returns it."""
    ann: dict[str, Any] = {"core:sample_start": int(sample_start)}
    optional = {
        "core:sample_count": None if sample_count is None else int(sample_count),
        "core:freq_lower_edge": None if freq_lower_edge is None else float(freq_lower_edge),
        "core:freq_upper_edge": None if freq_upper_edge is None else float(freq_upper_edge),
        "core:label": label,
        "core:comment": comment,
        TRUTH_KEY: truth,
    }
    ann.update({k: v for k, v in optional.items() if v is not None})
    ann.update(extra or {})
    meta["annotations"].append(ann)
    return ann


def validate(meta: dict[str, Any]) -> None:
    """Checks the keys the Rust types require. Raises :class:`SigmfError`."""
    glob = meta.get("global")
    if not isinstance(glob, dict):
        raise SigmfError("missing 'global' object")
    for key in ("core:datatype", "core:version"):
        if key not in glob:
            raise SigmfError(f"global: missing {key}")
    _datatype(glob["core:datatype"])
    if PROVENANCE_KEY in glob:
        _validate_provenance(glob[PROVENANCE_KEY], f"global.{PROVENANCE_KEY}")
    for section in ("captures", "annotations"):
        for i, item in enumerate(meta.get(section, [])):
            start = item.get("core:sample_start")
            if not isinstance(start, int) or isinstance(start, bool) or start < 0:
                raise SigmfError(f"{section}[{i}]: core:sample_start must be a non-negative int")
            if PROVENANCE_KEY in item:
                _validate_provenance(item[PROVENANCE_KEY], f"{section}[{i}].{PROVENANCE_KEY}")


def read_meta(path: str | Path) -> dict[str, Any]:
    """Reads and validates a ``.sigmf-meta`` file."""
    meta = json.loads(Path(path).read_text(encoding="utf-8"))
    meta.setdefault("captures", [])
    meta.setdefault("annotations", [])
    validate(meta)
    return meta


def write_meta(meta: dict[str, Any], path: str | Path) -> None:
    """Validates and writes pretty JSON. Captures and annotations are sorted by sample_start."""
    validate(meta)
    out = copy.deepcopy(meta)
    for section in ("captures", "annotations"):
        out[section] = sorted(out.get(section, []), key=lambda item: item["core:sample_start"])
    Path(path).write_text(json.dumps(out, indent=2) + "\n", encoding="utf-8")


def data_path(meta_path: str | Path) -> Path:
    """The ``.sigmf-data`` path paired with a ``.sigmf-meta`` path."""
    return Path(meta_path).with_suffix(".sigmf-data")


def _datatype(datatype: Any) -> tuple[str, bool]:
    try:
        return DATATYPES[datatype]
    except (KeyError, TypeError):
        raise SigmfError(f"unsupported core:datatype {datatype!r}") from None


def _validate_provenance(p: Any, where: str) -> None:
    if not isinstance(p, dict):
        raise SigmfError(f"{where}: must be an object")
    for key in _PROVENANCE_REQUIRED:
        if key not in p:
            raise SigmfError(f"{where}: missing {key}")
    tune = p["tune"]
    if not isinstance(tune, dict) or any(k not in tune for k in _TUNE_FIELDS):
        raise SigmfError(f"{where}.tune: needs {', '.join(_TUNE_FIELDS)}")
    if p["clock_source"] not in CLOCK_SOURCES:
        raise SigmfError(f"{where}: clock_source {p['clock_source']!r} not in {sorted(CLOCK_SOURCES)}")
    if p["timestamp_method"] not in TIMESTAMP_METHODS:
        raise SigmfError(
            f"{where}: timestamp_method {p['timestamp_method']!r} not in {sorted(TIMESTAMP_METHODS)}"
        )
