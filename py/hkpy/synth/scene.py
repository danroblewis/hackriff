"""Scene: an IQ buffer plus the ground truth that describes it, written as SigMF.

Conventions (repeated in every file's ``scenario`` truth annotation):

- **Normalised IQ.** Samples are complex with full scale 1.0 per component. ``ci8`` stores
  ``round(127 * x)`` clipped to [-128, 127]; ``cf32_le`` stores ``x`` clipped to [-1, 1].
- **dBFS.** ``10*log10(mean |x|^2)`` of normalised IQ, so a complex tone of amplitude 1.0 is 0 dBFS.
  ``*_dbfs_per_hz`` is that power divided by the bandwidth it is spread over.
- **Calibration.** ``P_dBm = P_dBFS + calibration_k_db`` (a stated constant per capture).
- **Frequencies** in truth and annotation edges are absolute RF Hz *as seen in the recording*
  (``core:frequency`` + baseband offset). ``rf_center_hz`` is the emitter's true frequency before any
  simulated LO error.
- **Roles.** Every ``hackriff:truth`` object has ``role`` and ``kind``. Roles: ``scenario`` (one per
  file, whole-file box, generator parameters), ``emission`` (a real signal a detector should find),
  ``artefact`` (a receiver artefact: spur, IM3 product, IQ image, DC, overload; explains a detection
  but is not an emitter), ``floor`` (a region of known noise floor), ``event`` (a change such as a
  floor step).
"""

from __future__ import annotations

import datetime as _dt
import hashlib
import inspect
import math
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import numpy as np

from hkpy import sigmf
from hkpy.synth import fill

GENERATOR = "hkpy.synth"
GENERATOR_VERSION = "0.1.0"
CI8_SCALE = 127.0
DEVICE_ID = "synthetic:hkpy.synth"
#: A capture is flagged ``overload`` when more than this fraction of its samples clipped.
OVERLOAD_CLIP_FRACTION = 1e-4
DBFS_REFERENCE = (
    "10*log10(mean |x|^2) with IQ normalised to full scale 1.0 per component "
    "(127 counts in ci8); a complex tone of amplitude 1.0 is 0 dBFS"
)
CALIBRATION_RULE = "P_dBm = P_dBFS + calibration_k_db"
SUPPORTED_DATATYPES = ("ci8", "cf32_le")
#: Per-capture clipped-sample count. T-002 moves clip counts out of Provenance into this key.
CLIP_COUNT_KEY = "hackriff:clip_count"
# Transitional: pass clip_count to sigmf.provenance() only while it still accepts it (pre-T-002).
_PROVENANCE_TAKES_CLIP_COUNT = "clip_count" in inspect.signature(sigmf.provenance).parameters


def db(x: float) -> float:
    return 10.0 * math.log10(x)


def undb(d: float) -> float:
    return 10.0 ** (d / 10.0)


def stable_int(text: str) -> int:
    """A process-independent 32-bit integer for seeding (Python's ``hash`` is salted)."""
    return int.from_bytes(hashlib.sha256(text.encode()).digest()[:4], "little")


def rng_for(seed: int, *names: str | int) -> np.random.Generator:
    """An independent generator per (seed, component name) so toggling one part leaves others intact."""
    key = [stable_int(str(n)) for n in names]
    return np.random.default_rng(np.random.SeedSequence(entropy=int(seed), spawn_key=key))


def quantisation_noise_dbfs(datatype: str) -> float | None:
    """Total quantisation-noise power over the full sample-rate bandwidth (uniform-error model)."""
    if datatype == "ci8":
        return db(2.0 * (1.0 / CI8_SCALE) ** 2 / 12.0)
    return None


def parse_utc(text: str) -> _dt.datetime:
    t = _dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
    if t.tzinfo is None:
        t = t.replace(tzinfo=_dt.UTC)
    return t.astimezone(_dt.UTC)


def format_utc(t: _dt.datetime) -> str:
    return t.astimezone(_dt.UTC).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def complex_noise(rng: np.random.Generator, n: int, power_dbfs: float) -> np.ndarray:
    """Circular complex Gaussian noise with mean |x|^2 = undb(power_dbfs)."""
    sigma = math.sqrt(undb(power_dbfs) / 2.0)
    return sigma * (rng.standard_normal(n) + 1j * rng.standard_normal(n))


@dataclass
class CaptureSeg:
    sample_start: int
    sample_count: int
    center_hz: float
    datetime: str
    calibration_k_db: float
    lna_db: float
    vga_db: float
    amp_on: bool
    #: Analogue noise density in dBFS/Hz, when the scenario knows it (used to decide IQ-image visibility).
    floor_dbfs_per_hz: float | None = None
    clip_count: int = 0
    #: ADC fill measured on the written samples, per-component sigma in LSB (T-625). ``None``
    #: for a datatype with no ADC, which a reader treats as ``under_filled``.
    noise_sigma_lsb: float | None = None


@dataclass
class Scene:
    """One SigMF recording under construction."""

    name: str
    scenario: str
    seed: int
    params: dict[str, Any]
    sample_rate: float
    n_samples: int
    datatype: str
    description: str
    use_cases: list[str]
    x: np.ndarray = field(init=False)
    captures: list[CaptureSeg] = field(default_factory=list)
    annotations: list[dict[str, Any]] = field(default_factory=list)
    impairments: list[dict[str, Any]] = field(default_factory=list)
    scenario_truth: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.datatype not in SUPPORTED_DATATYPES:
            raise ValueError(f"datatype must be one of {SUPPORTED_DATATYPES}, got {self.datatype!r}")
        self.x = np.zeros(self.n_samples, dtype=np.complex128)

    # ---- construction helpers -------------------------------------------------------------

    def add_capture(
        self,
        sample_start: int,
        sample_count: int,
        center_hz: float,
        start_utc: str,
        *,
        calibration_k_db: float,
        lna_db: float = 24.0,
        vga_db: float = 20.0,
        amp_on: bool = False,
    ) -> CaptureSeg:
        cap = CaptureSeg(
            sample_start=sample_start,
            sample_count=sample_count,
            center_hz=float(center_hz),
            datetime=start_utc,
            calibration_k_db=float(calibration_k_db),
            lna_db=lna_db,
            vga_db=vga_db,
            amp_on=amp_on,
        )
        self.captures.append(cap)
        return cap

    def capture_at(self, sample: int) -> CaptureSeg:
        for cap in reversed(self.captures):
            if sample >= cap.sample_start:
                return cap
        raise ValueError("no capture segment covers sample 0; add_capture first")

    def rng(self, *names: str | int) -> np.random.Generator:
        return rng_for(self.seed, self.scenario, self.name, *names)

    def time(self, start: int, count: int) -> np.ndarray:
        return (start + np.arange(count)) / self.sample_rate

    def add_samples(self, start: int, samples: np.ndarray) -> None:
        end = min(start + len(samples), self.n_samples)
        if start < 0 or start >= self.n_samples:
            raise ValueError(f"{self.name}: sample start {start} outside the recording")
        self.x[start:end] += samples[: end - start]

    def annotate(
        self,
        sample_start: int,
        sample_count: int,
        f_lo: float,
        f_hi: float,
        label: str,
        truth: dict[str, Any],
    ) -> dict[str, Any]:
        """Records an annotation. ``truth`` must carry ``role`` and ``kind``."""
        if "role" not in truth or "kind" not in truth:
            raise ValueError("truth needs role and kind")
        truth.setdefault("t_start_s", sample_start / self.sample_rate)
        truth.setdefault("duration_s", sample_count / self.sample_rate)
        ann = {
            "sample_start": int(sample_start),
            "sample_count": int(sample_count),
            "f_lo": float(f_lo),
            "f_hi": float(f_hi),
            "label": label,
            "truth": truth,
        }
        self.annotations.append(ann)
        return ann

    def add_floor(self, sample_start: int, sample_count: int, noise_dbfs_per_hz: float,
                  f_lo: float | None = None, f_hi: float | None = None, label: str = "noise-floor",
                  **extra: Any) -> dict[str, Any]:
        """A ``floor`` truth region. Powers are integrated over the annotation's frequency box."""
        cap = self.capture_at(sample_start)
        if f_lo is None or f_hi is None:
            f_lo = cap.center_hz - self.sample_rate / 2
            f_hi = cap.center_hz + self.sample_rate / 2
        bw = f_hi - f_lo
        floor_dbfs = noise_dbfs_per_hz + db(bw)
        truth: dict[str, Any] = {
            "role": "floor",
            "kind": "noise-floor",
            "floor_dbfs": floor_dbfs,
            "floor_dbfs_per_hz": noise_dbfs_per_hz,
            "calibration_k_db": cap.calibration_k_db,
            "floor_dbm": floor_dbfs + cap.calibration_k_db,
            "floor_dbm_per_hz": noise_dbfs_per_hz + cap.calibration_k_db,
            "bandwidth_hz": bw,
        }
        q = quantisation_noise_dbfs(self.datatype)
        if q is not None:
            q_box = q - db(self.sample_rate) + db(bw)
            truth["quantisation_noise_dbfs"] = q_box
            truth["expected_floor_dbfs"] = db(undb(floor_dbfs) + undb(q_box))
        else:
            truth["expected_floor_dbfs"] = floor_dbfs
        truth.update(extra)
        return self.annotate(sample_start, sample_count, f_lo, f_hi, label, truth)

    def emission_truth(self, cap: CaptureSeg, offset_hz: float, bandwidth_hz: float,
                       power_dbfs: float, **fields: Any) -> dict[str, Any]:
        """Common ``emission`` fields; the caller adds modulation-specific ones."""
        truth: dict[str, Any] = {
            "role": "emission",
            "center_hz": cap.center_hz + offset_hz,
            "rf_center_hz": cap.center_hz + offset_hz,
            "offset_hz": offset_hz,
            "bandwidth_hz": bandwidth_hz,
            "power_dbfs": power_dbfs,
            "power_dbm": power_dbfs + cap.calibration_k_db,
        }
        if cap.floor_dbfs_per_hz is not None and bandwidth_hz > 0:
            truth["snr_db"] = power_dbfs - (cap.floor_dbfs_per_hz + db(bandwidth_hz))
        truth.update(fields)
        return truth

    def annotations_in(self, cap: CaptureSeg, roles: tuple[str, ...]) -> list[dict[str, Any]]:
        end = cap.sample_start + cap.sample_count
        return [a for a in self.annotations
                if cap.sample_start <= a["sample_start"] < end and a["truth"]["role"] in roles]

    def scale_powers(self, cap: CaptureSeg, gain_db: float) -> None:
        """Applies a linear gain change (dB) to the power truth of everything already in ``cap``."""
        if gain_db == 0.0:
            return
        if cap.floor_dbfs_per_hz is not None:
            cap.floor_dbfs_per_hz += gain_db
        for ann in self.annotations_in(cap, ("emission", "artefact", "floor", "event")):
            t = ann["truth"]
            for key in ("power_dbfs", "power_dbm", "floor_dbfs", "floor_dbfs_per_hz", "floor_dbm",
                        "floor_dbm_per_hz", "floor_before_dbfs_per_hz", "floor_after_dbfs_per_hz",
                        "floor_before_dbm_per_hz", "floor_after_dbm_per_hz"):
                if t.get(key) is not None:
                    t[key] += gain_db
            if t["role"] == "floor":
                q = t.get("quantisation_noise_dbfs")
                t["expected_floor_dbfs"] = (
                    t["floor_dbfs"] if q is None else db(undb(t["floor_dbfs"]) + undb(q))
                )

    # ---- output ---------------------------------------------------------------------------

    def quantise(self) -> tuple[bytes, np.ndarray, np.ndarray]:
        """Returns the data bytes, a per-sample clipped mask (either component saturated) and the
        written samples as an ``(n, 2)`` component array (``int8`` for ``ci8``)."""
        iq = np.stack([self.x.real, self.x.imag], axis=-1)
        if self.datatype == "ci8":
            scaled = iq * CI8_SCALE
            clipped = ((scaled > 127.5) | (scaled < -128.5)).any(axis=-1)
            data = np.clip(np.rint(scaled), -128, 127).astype(np.int8)
        else:
            clipped = (np.abs(iq) > 1.0).any(axis=-1)
            data = np.clip(iq, -1.0, 1.0).astype("<f4")
        return data.reshape(-1).tobytes(), clipped, data

    def write(self, out_dir: Path) -> Path:
        data, clipped, written = self.quantise()
        for cap in self.captures:
            span = slice(cap.sample_start, cap.sample_start + cap.sample_count)
            cap.clip_count = int(clipped[span].sum())
            cap.noise_sigma_lsb = self._fill_sigma_lsb(written[span])
        self._annotate_overload(clipped)

        total_clips = int(clipped.sum())
        first = self.captures[0]
        glob_prov = self._provenance(first, total_clips, self.n_samples)
        total_sigma = self._fill_sigma_lsb(written)
        lo = min(c.center_hz for c in self.captures) - self.sample_rate / 2
        hi = max(c.center_hz for c in self.captures) + self.sample_rate / 2
        meta = sigmf.new_meta(
            self.datatype,
            self.sample_rate,
            description=self.description,
            author="hackriff synthetic generator",
            license="https://spdx.org/licenses/CC0-1.0.html",
            hw="synthetic (no hardware)",
            recorder=f"{GENERATOR} {GENERATOR_VERSION}",
            provenance=glob_prov,
        )
        multi = len(self.captures) > 1
        for cap in self.captures:
            sigmf.add_capture(
                meta,
                cap.sample_start,
                frequency=cap.center_hz,
                datetime=cap.datetime,
                provenance=self._provenance(cap, cap.clip_count, cap.sample_count) if multi else None,
                extra={CLIP_COUNT_KEY: cap.clip_count},
            )
        scenario_truth = {
            "role": "scenario",
            "kind": "scenario",
            "scenario": self.scenario,
            "recording": self.name,
            "seed": self.seed,
            "params": self.params,
            "generator": GENERATOR,
            "generator_version": GENERATOR_VERSION,
            "use_cases": self.use_cases,
            "datatype": self.datatype,
            "sample_rate_hz": self.sample_rate,
            "n_samples": self.n_samples,
            "duration_s": self.n_samples / self.sample_rate,
            "dbfs_reference": DBFS_REFERENCE,
            "calibration_rule": CALIBRATION_RULE,
            "quantisation_noise_dbfs": quantisation_noise_dbfs(self.datatype),
            "clip_count": total_clips,
            "clip_fraction": total_clips / self.n_samples,
            # ADC fill (T-625). Hidden truth beside the recorded provenance value, so a test can
            # tell "the generator knew" from "the reader measured".
            "noise_sigma_lsb": total_sigma,
            "fill_bucket": fill.fill_bucket(total_sigma, total_clips / max(self.n_samples, 1)),
            "overload": glob_prov["overload"],
            "overload_rule": f"clip_fraction > {OVERLOAD_CLIP_FRACTION:g}",
            "impairments": self.impairments,
        }
        scenario_truth.update(self.scenario_truth)
        sigmf.add_annotation(meta, 0, sample_count=self.n_samples, freq_lower_edge=lo,
                             freq_upper_edge=hi, label="scenario", truth=_jsonable(scenario_truth))
        for ann in self.annotations:
            sigmf.add_annotation(
                meta,
                ann["sample_start"],
                sample_count=ann["sample_count"],
                freq_lower_edge=ann["f_lo"],
                freq_upper_edge=ann["f_hi"],
                label=ann["label"],
                truth=_jsonable(ann["truth"]),
            )
        out_dir.mkdir(parents=True, exist_ok=True)
        meta_path = out_dir / f"{self.name}.sigmf-meta"
        sigmf.data_path(meta_path).write_bytes(data)
        sigmf.write_meta(meta, meta_path)
        return meta_path

    def _provenance(self, cap: CaptureSeg, clips: int, count: int) -> dict[str, Any]:
        kwargs: dict[str, Any] = {
            "center_hz": cap.center_hz,
            "sample_rate_hz": self.sample_rate,
            "lna_db": cap.lna_db,
            "vga_db": cap.vga_db,
            "amp_on": cap.amp_on,
            "bandwidth_hz": 0.75 * self.sample_rate,
            "overload": clips > OVERLOAD_CLIP_FRACTION * max(count, 1),
            "clock_source": "internal",
            "clock_locked": True,
            "timestamp_method": "synthetic",
            "antenna_port": "synthetic",
            "timestamp_error_budget_ns": 0,
            "noise_sigma_lsb": cap.noise_sigma_lsb,
        }
        if _PROVENANCE_TAKES_CLIP_COUNT:
            kwargs["clip_count"] = clips
        return sigmf.provenance(DEVICE_ID, **kwargs)

    def _fill_sigma_lsb(self, written: np.ndarray) -> float | None:
        """ADC fill for this span: per-component sigma in LSB, or ``None`` when there is no ADC.

        A ``cf32_le`` recording was never converted, so there is no fill to report and nothing may
        invent one: the reader then classifies it ``under_filled`` and credits calibrated metrics
        0 bits, which is the fail-closed direction (ADR-0015 section 13.3). T-547's float control
        is exactly this case, and it is the control precisely because it skipped the ADC.
        """
        if self.datatype != "ci8":
            return None
        sigma, _ = fill.measure_fill(written)
        return sigma

    def _annotate_overload(self, clipped: np.ndarray, max_regions: int = 256) -> None:
        idx = np.flatnonzero(clipped)
        if idx.size == 0:
            return
        gap = max(1, int(1e-3 * self.sample_rate))
        breaks = np.flatnonzero(np.diff(idx) > gap)
        starts = np.concatenate([[idx[0]], idx[breaks + 1]])
        ends = np.concatenate([idx[breaks], [idx[-1]]]) + 1
        for s, e in list(zip(starts, ends, strict=True))[:max_regions]:
            cap = self.capture_at(int(s))
            self.annotate(
                int(s), int(e - s), cap.center_hz - self.sample_rate / 2,
                cap.center_hz + self.sample_rate / 2, "overload",
                {"role": "artefact", "kind": "overload",
                 "clipped_samples": int(clipped[s:e].sum()), "merge_gap_samples": gap},
            )


def _jsonable(v: Any) -> Any:
    """Converts numpy scalars/arrays and non-finite floats to plain JSON values."""
    if isinstance(v, dict):
        return {str(k): _jsonable(x) for k, x in v.items()}
    if isinstance(v, (list, tuple)):
        return [_jsonable(x) for x in v]
    if isinstance(v, np.ndarray):
        return [_jsonable(x) for x in v.tolist()]
    if isinstance(v, (np.bool_,)):
        return bool(v)
    if isinstance(v, np.integer):
        return int(v)
    if isinstance(v, (np.floating, float)):
        f = float(v)
        return f if math.isfinite(f) else None
    return v
