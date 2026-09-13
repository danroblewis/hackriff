"""Shared helpers for the fixture tooling in ``py/fixtures/`` (scripts, not a package).

Run the scripts with the hkpy environment from the repo root, e.g.
``uv run --project py python py/fixtures/verify.py``. Each script puts this directory on
``sys.path`` itself (Python does that for the script's own directory).
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

import numpy as np

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(REPO / "py"))

from hkpy import sigmf  # noqa: E402

FIXTURES = REPO / "fixtures"
MANIFEST = FIXTURES / "manifest.json"
MANIFEST_VERSION = 2
LFS_POINTER_PREFIX = b"version https://git-lfs.github.com/spec/v1"
CLIP_COUNT_KEY = "hackriff:clip_count"
#: Same rule as the synthetic generator (hkpy/synth/scene.py): overload iff clip fraction > 1e-4.
OVERLOAD_CLIP_FRACTION = 1e-4
#: Committed ``.sigmf-data`` cap (fixtures/README.md), read as 25 000 000 bytes to be safe.
MAX_COMMITTED_BYTES = 25_000_000
#: MAX2837 baseband filter table (libhackrf). hackrf_transfer/hackrf_sweep without -b pick
#: hackrf_compute_baseband_filter_bw(0.75 * fs): the largest entry <= the request, else the first.
MAX2837_BANDWIDTHS_HZ = (1.75e6, 2.5e6, 3.5e6, 5e6, 5.5e6, 6e6, 7e6, 8e6, 9e6, 10e6, 12e6, 14e6,
                         15e6, 20e6, 24e6, 28e6)


def store_dir(arg: str | None = None) -> Path:
    """The external fixture store: ``arg``, else ``$HACKRIFF_FIXTURE_STORE``, else fixtures/store."""
    return Path(arg or os.environ.get("HACKRIFF_FIXTURE_STORE") or FIXTURES / "store").resolve()


def sha256_file(path: Path, chunk: int = 8 << 20) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(chunk):
            h.update(block)
    return h.hexdigest()


def is_lfs_pointer(path: Path) -> bool:
    with open(path, "rb") as f:
        return f.read(len(LFS_POINTER_PREFIX)) == LFS_POINTER_PREFIX


def hackrf_default_bandwidth(sample_rate_hz: float) -> float:
    want = 0.75 * sample_rate_hz
    below = [b for b in MAX2837_BANDWIDTHS_HZ if b <= want]
    return below[-1] if below else MAX2837_BANDWIDTHS_HZ[0]


# ---- samples ---------------------------------------------------------------------------------


def n_samples(data_path: Path, datatype: str) -> int:
    return os.path.getsize(data_path) // sigmf.bytes_per_sample(datatype)


def read_ci8(data_path: Path, start: int, count: int) -> np.ndarray:
    """``count`` complex samples (float32, code units) from ``start``."""
    raw = np.memmap(data_path, dtype=np.int8, mode="r")
    a = np.asarray(raw[2 * start : 2 * (start + count)], dtype=np.float32).reshape(-1, 2)
    return (a[:, 0] + 1j * a[:, 1]).astype(np.complex64)


def clipped_mask_ci8(raw_pairs: np.ndarray) -> np.ndarray:
    """Per-sample clip mask for ci8 ``(n, 2)`` int8: either component at a rail (-128 or 127).
    Matches the generator's definition (a component driven past full scale)."""
    return ((raw_pairs == 127) | (raw_pairs == -128)).any(axis=1)


def count_clipped(data_path: Path, datatype: str, start: int, count: int,
                  chunk: int = 1 << 22) -> int:
    if datatype not in ("ci8", "ri8"):
        raise ValueError(f"clip counting implemented for 8-bit types only, not {datatype}")
    width = 2 if datatype == "ci8" else 1
    raw = np.memmap(data_path, dtype=np.int8, mode="r")
    total = 0
    for a in range(start, start + count, chunk):
        b = min(start + count, a + chunk)
        pairs = np.asarray(raw[width * a : width * b]).reshape(-1, width)
        total += int(((pairs == 127) | (pairs == -128)).any(axis=1).sum())
    return total


# ---- metadata --------------------------------------------------------------------------------


def parse_datetime(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(timezone.utc)


def format_datetime(dt: datetime) -> str:
    """SigMF ``core:datetime``: ISO-8601 UTC with ``Z``, microsecond precision."""
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def shift_datetime(s: str, seconds: float) -> str:
    return format_datetime(parse_datetime(s) + timedelta(seconds=seconds))


def quantisation_floor_dbfs_per_hz(sample_rate_hz: float) -> float:
    """8-bit complex quantisation noise density: uniform error, 1/12 code^2 per component."""
    return 10 * np.log10((2 / 12) / 127**2 / sample_rate_hz)


def floor_psd(data_path: Path, start: int, count: int, nfft: int = 1024,
              max_frames: int = 4096) -> np.ndarray:
    """Mean periodogram per bin (fftshifted), full-scale-normalised power per bin (not per Hz),
    Hann window, from up to ``max_frames`` frames spread evenly across the span."""
    n_frames = count // nfft
    take = np.linspace(0, n_frames - 1, min(n_frames, max_frames)).astype(int)
    win = np.hanning(nfft).astype(np.float32)
    acc = np.zeros(nfft)
    for k0 in range(0, len(take), 256):
        rows = [read_ci8(data_path, start + int(k) * nfft, nfft) for k in take[k0 : k0 + 256]]
        X = np.fft.fft(np.stack(rows) / 127.0 * win, axis=-1)
        acc += (np.abs(X) ** 2).sum(0)
    return np.fft.fftshift(acc / len(take) / np.sum(win**2))


def measured_floor_dbfs_per_hz(data_path: Path, sample_rate_hz: float, start: int, count: int,
                               nfft: int = 1024, band_fraction: float = 0.7) -> float:
    """Median over the central ``band_fraction`` of bins (DC +-3 bins excluded) of the mean PSD."""
    psd = floor_psd(data_path, start, count, nfft)
    f = (np.arange(nfft) - nfft // 2)
    sel = (np.abs(f) <= band_fraction * nfft / 2) & (np.abs(f) > 3)
    return float(10 * np.log10(np.median(psd[sel]) / sample_rate_hz))


def is_quantisation_limited(data_path: Path, sample_rate_hz: float, start: int, count: int) -> bool:
    """docs/sigmf-extension.md: floor within 3 dB of the ADC quantisation floor."""
    floor = measured_floor_dbfs_per_hz(data_path, sample_rate_hz, start, count)
    return floor < quantisation_floor_dbfs_per_hz(sample_rate_hz) + 3.0


def normalise_store_provenance(p: dict[str, Any], clip_count: int, n: int,
                               quantisation_limited: bool) -> dict[str, Any]:
    """Store captures (2026-09-13) wrote provenance with nulls and free-text enums. Map it onto
    the ``hackriff:provenance`` schema (docs/sigmf-extension.md), dropping unknown values rather
    than writing null:

    - ``device_id`` ``hackrf-<serial>`` -> ``hackrf:<serial>``;
    - ``tune.bandwidth_hz`` null -> the hackrf_transfer default filter for the sample rate;
    - ``clock_source`` "internal TCXO/crystal" -> ``internal`` (locked by definition);
    - ``timestamp_method`` "host-arrival-time ..." -> ``host-arrival``;
      ``timestamp_error_budget_s`` -> ``timestamp_error_budget_ns``;
    - ``overload`` = clip fraction > 1e-4 over the ``n`` samples covered (legacy ``clip_count``
      dropped: counts live on the capture as ``hackriff:clip_count``);
    - ``quantisation_limited`` as measured by the caller when the source lacks it.
    """
    if all(k in p and p[k] is not None for k in ("overload", "quantisation_limited")) and \
            p.get("clock_source") in sigmf.CLOCK_SOURCES:
        out = {k: v for k, v in p.items() if k != "clip_count" and v is not None}
        return out
    tune = p["tune"]
    fs = float(tune["sample_rate_hz"])
    device = str(p["device_id"])
    if device.startswith("hackrf-"):
        device = "hackrf:" + device[len("hackrf-"):]
    clock = str(p.get("clock_source") or "")
    method = str(p.get("timestamp_method") or "")
    budget_s = p.get("timestamp_error_budget_s")
    out = sigmf.provenance(
        device,
        center_hz=float(tune["center_hz"]),
        sample_rate_hz=fs,
        lna_db=float(tune["lna_db"]),
        vga_db=float(tune["vga_db"]),
        amp_on=bool(tune["amp_on"]),
        bandwidth_hz=float(tune["bandwidth_hz"] or hackrf_default_bandwidth(fs)),
        overload=clip_count > OVERLOAD_CLIP_FRACTION * max(n, 1),
        quantisation_limited=bool(quantisation_limited),
        clock_source="internal" if clock.startswith("internal") else clock,
        clock_locked=True if clock.startswith("internal") else bool(p.get("clock_locked")),
        timestamp_method="host-arrival" if method.startswith("host-arrival") else (method or "unknown"),
        temperature_c=p.get("temperature_c"),
        calibration_state_ref=p.get("calibration_state_ref"),
        spur_mask_ref=p.get("spur_mask_ref"),
        timestamp_error_budget_ns=None if budget_s is None else int(round(float(budget_s) * 1e9)),
    )
    return out


def load_json(path: Path) -> Any:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def dump_json(obj: Any, path: Path) -> None:
    Path(path).write_text(json.dumps(obj, indent=2, default=_json_default) + "\n", encoding="utf-8")


def _json_default(o: Any) -> Any:
    if isinstance(o, np.integer):
        return int(o)
    if isinstance(o, np.floating):
        return float(o)
    if isinstance(o, np.bool_):
        return bool(o)
    if isinstance(o, np.ndarray):
        return o.tolist()
    raise TypeError(f"not JSON serialisable: {type(o)}")


def rel_to_fixtures(path: Path) -> str:
    return Path(os.path.relpath(Path(path).resolve(), FIXTURES.resolve())).as_posix()
