"""Cut a time window out of a SigMF recording, sample-exact.

    uv run --project py python py/fixtures/trim.py SRC.sigmf-meta DST.sigmf-meta \
        --start-s 17.0 --duration-s 1.2 [--description TEXT]

Semantics:
- The window is ``[round(start_s * fs), + round(duration_s * fs))`` samples; the data bytes are
  copied verbatim (no requantisation).
- Each capture segment overlapping the window is kept. Its ``core:sample_start`` becomes the
  offset in the new file; ``core:global_index`` is the sample index in the *original* recording
  (source ``core:global_index`` if present, else its ``core:sample_start``, plus the offset into
  the segment); ``core:datetime`` advances by that offset / fs.
- ``hackriff:clip_count`` is recomputed per kept segment (8-bit types: a component at -128 or 127).
- Provenance is normalised to the ``hackriff:provenance`` schema (``fxlib``); ``overload`` is
  recomputed for the span each provenance object covers (global: the window) and a missing
  ``quantisation_limited`` is measured on that span. Legacy ``clip_count`` inside provenance is
  dropped.
- Annotations overlapping the window are clipped to it and shifted.
"""

from __future__ import annotations

import argparse
import copy
import sys
from pathlib import Path
from typing import Any

import fxlib
from fxlib import CLIP_COUNT_KEY, sigmf


def trim(src_meta: Path, dst_meta: Path, *, start_s: float | None = None,
         duration_s: float | None = None, start_sample: int | None = None,
         count: int | None = None, description: str | None = None,
         global_extra: dict[str, Any] | None = None) -> dict[str, Any]:
    src_meta, dst_meta = Path(src_meta), Path(dst_meta)
    meta = fxlib.load_json(src_meta)
    glob = meta["global"]
    datatype = glob["core:datatype"]
    fs = float(glob["core:sample_rate"])
    bps = sigmf.bytes_per_sample(datatype)
    src_data = sigmf.data_path(src_meta)
    n_total = fxlib.n_samples(src_data, datatype)

    s0 = int(start_sample) if start_sample is not None else int(round(float(start_s) * fs))
    n = int(count) if count is not None else int(round(float(duration_s) * fs))
    if s0 < 0 or n <= 0 or s0 + n > n_total:
        raise ValueError(f"window [{s0}, {s0 + n}) outside [0, {n_total})")

    dst_meta.parent.mkdir(parents=True, exist_ok=True)
    dst_data = sigmf.data_path(dst_meta)
    with open(src_data, "rb") as fi, open(dst_data, "wb") as fo:
        fi.seek(s0 * bps)
        remaining = n * bps
        while remaining:
            block = fi.read(min(remaining, 8 << 20))
            if not block:
                raise OSError(f"{src_data}: short read")
            fo.write(block)
            remaining -= len(block)

    def clips(a: int, b: int) -> int:
        return fxlib.count_clipped(src_data, datatype, a, b - a) if b > a else 0

    def fix(p: dict[str, Any], a: int, b: int) -> dict[str, Any]:
        ql = p.get("quantisation_limited")
        if ql is None:
            ql = fxlib.is_quantisation_limited(src_data, fs, a, b - a) if datatype == "ci8" else False
        return fxlib.normalise_store_provenance(p, clips(a, b), b - a, ql)

    out_glob = copy.deepcopy(glob)
    out_glob["core:version"] = glob.get("core:version", sigmf.SIGMF_VERSION)
    exts = [e for e in out_glob.get("core:extensions", []) if e.get("name") != sigmf.HACKRIFF_EXTENSION]
    exts.append({"name": sigmf.HACKRIFF_EXTENSION, "version": sigmf.HACKRIFF_EXTENSION_VERSION,
                 "optional": True})
    out_glob["core:extensions"] = exts
    if sigmf.PROVENANCE_KEY in out_glob:
        out_glob[sigmf.PROVENANCE_KEY] = fix(out_glob[sigmf.PROVENANCE_KEY], s0, s0 + n)
    if description is not None:
        out_glob["core:description"] = description
    out_glob.update(global_extra or {})

    caps = sorted(meta.get("captures", []), key=lambda c: c["core:sample_start"])
    out_caps = []
    for i, cap in enumerate(caps):
        seg_a = cap["core:sample_start"]
        seg_b = caps[i + 1]["core:sample_start"] if i + 1 < len(caps) else n_total
        a, b = max(seg_a, s0), min(seg_b, s0 + n)
        if a >= b:
            continue
        c = copy.deepcopy(cap)
        offset = a - seg_a
        c["core:sample_start"] = a - s0
        c["core:global_index"] = int(cap.get("core:global_index", seg_a)) + offset
        if "core:datetime" in cap:
            c["core:datetime"] = fxlib.shift_datetime(cap["core:datetime"], offset / fs)
        c[CLIP_COUNT_KEY] = clips(a, b)
        if sigmf.PROVENANCE_KEY in c:
            c[sigmf.PROVENANCE_KEY] = fix(c[sigmf.PROVENANCE_KEY], a, b)
        out_caps.append(c)

    out_anns = []
    for ann in meta.get("annotations", []):
        a0 = ann["core:sample_start"]
        a1 = a0 + ann["core:sample_count"] if "core:sample_count" in ann else n_total
        a, b = max(a0, s0), min(a1, s0 + n)
        if a >= b:
            continue
        x = copy.deepcopy(ann)
        x["core:sample_start"] = a - s0
        if "core:sample_count" in ann:
            x["core:sample_count"] = b - a
        out_anns.append(x)

    out = {"global": out_glob, "captures": out_caps, "annotations": out_anns}
    sigmf.write_meta(out, dst_meta)
    return out


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--start-s", type=float, default=0.0)
    ap.add_argument("--duration-s", type=float, required=True)
    ap.add_argument("--description")
    args = ap.parse_args(argv)
    out = trim(Path(args.src), Path(args.dst), start_s=args.start_s, duration_s=args.duration_s,
               description=args.description)
    print(f"wrote {args.dst}: captures {len(out['captures'])}, annotations {len(out['annotations'])}, "
          f"clip_count {sum(c.get(CLIP_COUNT_KEY, 0) for c in out['captures'])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
