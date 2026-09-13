"""Fixture tooling in ``py/fixtures/``: sample-exact trimming, manifest verification, fetch,
annotation, the promoted RDS reference decoder, and the committed manifest itself."""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

import numpy as np
import pytest

FIXTURE_TOOLS = Path(__file__).resolve().parents[1] / "fixtures"
sys.path.insert(0, str(FIXTURE_TOOLS))

import annotate  # noqa: E402
import fetch  # noqa: E402
import fxlib  # noqa: E402
import rds_ref  # noqa: E402
import trim  # noqa: E402
import verify  # noqa: E402
from hkpy import sigmf  # noqa: E402

FS = 1000.0

#: Provenance exactly as the 2026-09-13 store captures wrote it (nulls, free-text enums).
STORE_PROVENANCE = {
    "device_id": "hackrf-0000000000000000d2b861dc263bc293",
    "tune": {"center_hz": 915e6, "sample_rate_hz": FS, "lna_db": 24, "vga_db": 30, "amp_on": True,
             "bandwidth_hz": None},
    "clip_count": 0, "overload": None, "temperature_c": None,
    "antenna_port": "unknown (as attached by user; not changed)",
    "clock_source": "internal TCXO/crystal", "clock_locked": None, "calibration_state_ref": None,
    "spur_mask_ref": None, "timestamp_method": "host-arrival-time (capture start, NTP host clock)",
    "timestamp_error_budget_s": 0.5,
}


def write_source(tmp_path: Path, n: int = 10_000) -> tuple[Path, np.ndarray]:
    rng = np.random.default_rng(7)
    pairs = rng.integers(-20, 21, size=(n, 2)).astype(np.int8)
    for idx in (100, 2500, 2501, 6000, 9999):  # clipped samples: one rail at full scale
        pairs[idx, idx % 2] = 127 if idx % 3 else -128
    data = tmp_path / "src.sigmf-data"
    pairs.reshape(-1).tofile(data)
    meta = {
        "global": {"core:datatype": "ci8", "core:sample_rate": FS, "core:version": "1.2.0",
                   "hackriff:provenance": STORE_PROVENANCE, "hackriff:antenna": "unknown"},
        "captures": [
            {"core:sample_start": 0, "core:frequency": 915e6, "core:datetime": "2026-09-13T11:08:46.357165+00:00"},
            {"core:sample_start": 4000, "core:frequency": 916e6, "core:datetime": "2026-09-13T11:08:50.357165Z",
             "core:global_index": 123_456},
        ],
        "annotations": [
            {"core:sample_start": 1000, "core:sample_count": 2000, "core:label": "a"},
            {"core:sample_start": 8000, "core:sample_count": 100, "core:label": "outside"},
        ],
    }
    (tmp_path / "src.sigmf-meta").write_text(json.dumps(meta))
    return tmp_path / "src.sigmf-meta", pairs


def test_trim_is_sample_exact_with_consistent_metadata(tmp_path):
    src, pairs = write_source(tmp_path)
    dst = tmp_path / "out" / "cut.sigmf-meta"
    out = trim.trim(src, dst, start_s=2.0, duration_s=3.0)  # samples [2000, 5000)

    got = np.fromfile(sigmf.data_path(dst), dtype=np.int8).reshape(-1, 2)
    assert np.array_equal(got, pairs[2000:5000])

    meta = sigmf.read_meta(dst)  # validates provenance against the schema
    caps = meta["captures"]
    assert [c["core:sample_start"] for c in caps] == [0, 2000]
    assert [c["core:global_index"] for c in caps] == [2000, 123_456]
    assert caps[0]["core:datetime"] == "2026-09-13T11:08:48.357165Z"
    assert caps[1]["core:datetime"] == "2026-09-13T11:08:50.357165Z"
    assert [c["hackriff:clip_count"] for c in caps] == [2, 0]  # 2500, 2501 in segment 0
    assert [c["core:frequency"] for c in caps] == [915e6, 916e6]

    prov = meta["global"]["hackriff:provenance"]
    assert prov["device_id"] == "hackrf:0000000000000000d2b861dc263bc293"
    assert "clip_count" not in prov
    assert prov["overload"] is True  # 2 / 3000 > 1e-4
    assert prov["clock_source"] == "internal" and prov["clock_locked"] is True
    assert prov["timestamp_method"] == "host-arrival"
    assert prov["timestamp_error_budget_ns"] == 500_000_000
    assert prov["tune"]["bandwidth_hz"] == 1.75e6
    assert isinstance(prov["quantisation_limited"], bool)
    assert not any(v is None for v in prov.values())
    assert meta["global"]["hackriff:antenna"] == "unknown"

    [ann] = meta["annotations"]
    assert (ann["core:sample_start"], ann["core:sample_count"]) == (0, 1000)
    assert out["annotations"] == meta["annotations"]


def test_trim_window_inside_second_segment_offsets_global_index(tmp_path):
    src, pairs = write_source(tmp_path)
    dst = tmp_path / "cut2.sigmf-meta"
    trim.trim(src, dst, start_sample=5000, count=5000)
    meta = sigmf.read_meta(dst)
    [cap] = meta["captures"]
    assert cap["core:sample_start"] == 0
    assert cap["core:global_index"] == 123_456 + 1000
    assert cap["core:datetime"] == "2026-09-13T11:08:51.357165Z"
    assert cap["hackriff:clip_count"] == 2  # 6000 and 9999
    assert np.array_equal(np.fromfile(sigmf.data_path(dst), dtype=np.int8).reshape(-1, 2), pairs[5000:])


def test_trim_rejects_windows_outside_the_recording(tmp_path):
    src, _ = write_source(tmp_path)
    with pytest.raises(ValueError):
        trim.trim(src, tmp_path / "bad.sigmf-meta", start_s=9.0, duration_s=2.0)


def test_hackrf_default_bandwidth_rounds_down():
    assert fxlib.hackrf_default_bandwidth(2.4e6) == 1.75e6
    assert fxlib.hackrf_default_bandwidth(2e6) == 1.75e6
    assert fxlib.hackrf_default_bandwidth(10e6) == 7e6
    assert fxlib.hackrf_default_bandwidth(20e6) == 15e6


def test_annotate_fills_times_and_rejects_unknown_roles(tmp_path):
    src, _ = write_source(tmp_path)
    dst = tmp_path / "ann.sigmf-meta"
    trim.trim(src, dst, start_s=0.0, duration_s=2.0)
    annotate.annotate(dst, {"annotations": [
        {"sample_start": 500, "sample_count": 250, "truth": {"role": "emission", "kind": "cw"}},
        {"truth": {"role": "floor", "kind": "noise-floor", "t_start_s": 1.5, "duration_s": 9.0}},
    ]})
    truths = [a for a in sigmf.read_meta(dst)["annotations"] if sigmf.TRUTH_KEY in a]
    assert [(a["core:sample_start"], a["core:sample_count"]) for a in truths] == [(500, 250), (1500, 500)]
    assert truths[0][sigmf.TRUTH_KEY]["t_start_s"] == 0.5
    assert truths[1][sigmf.TRUTH_KEY]["duration_s"] == 0.5  # clipped to the file
    with pytest.raises(annotate.TruthError):
        annotate.annotate(dst, {"annotations": [{"sample_start": 0, "sample_count": 1,
                                                 "truth": {"role": "recording", "kind": "x"}}]})


def make_manifest(root: Path, entries: list[dict]) -> Path:
    path = root / "manifest.json"
    path.write_text(json.dumps({"version": fxlib.MANIFEST_VERSION, "store": "fixtures/store", "entries": entries}))
    return path


def file_entry(root: Path, rel: str, content: bytes, status: str = "committed") -> dict:
    p = root / rel if not rel.startswith("store/") else root / "store" / rel[len("store/"):]
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content)
    return {"name": Path(rel).stem, "status": status, "path": rel, "size_bytes": len(content),
            "sha256": fxlib.sha256_file(p)}


def test_verify_accepts_matching_files_and_reports_problems(tmp_path, monkeypatch):
    good = file_entry(tmp_path, "hackrf/x/a.sigmf-data", b"\x01\x02" * 50)
    ext_present = file_entry(tmp_path, "store/d/big.sigmf-data", b"\x00" * 64, status="external")
    ext_missing = {"name": "gone", "status": "external", "path": "store/d/gone.sigmf-data",
                   "size_bytes": 1, "sha256": "0" * 64}
    blocked = {"name": "adsb", "status": "blocked", "reason": "antenna"}
    m = make_manifest(tmp_path, [good, ext_present, ext_missing, blocked])
    problems, notes = verify.verify(m, store=tmp_path / "store", external=True)
    assert problems == [] and len(notes) == 1 and "gone" in notes[0]
    problems, _ = verify.verify(m, store=tmp_path / "store", external=True, require_external=True)
    assert len(problems) == 1

    (tmp_path / "hackrf/x/a.sigmf-data").write_bytes(b"\x01\x03" * 50)  # same size, other bytes
    lfs = file_entry(tmp_path, "hackrf/x/b.sigmf-data",
                     b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 9\n")
    no_reason = {"name": "t", "status": "blocked"}
    m = make_manifest(tmp_path, [good, lfs, no_reason])
    problems, _ = verify.verify(m, store=tmp_path / "store")
    assert any("sha256" in p for p in problems)
    assert any("LFS pointer" in p for p in problems)
    assert any("without reason" in p for p in problems)

    monkeypatch.setattr(fxlib, "MAX_COMMITTED_BYTES", 10)
    (tmp_path / "hackrf/x/a.sigmf-data").write_bytes(b"\x01\x02" * 50)
    problems, _ = verify.verify(make_manifest(tmp_path, [good]), store=tmp_path / "store")
    assert any("exceeds" in p for p in problems)


def test_fetch_copies_with_checksum_and_refuses_corrupt_sources(tmp_path):
    src_root, dst_root = tmp_path / "src", tmp_path / "dst"
    e = file_entry(src_root, "store/d/orig.sigmf-data", b"iq" * 1000, status="external")
    m = make_manifest(tmp_path, [e])
    assert fetch.fetch(m, src=src_root / "store", dst=dst_root) == []
    assert (dst_root / "d/orig.sigmf-data").read_bytes() == b"iq" * 1000
    assert fetch.fetch(m, src=dst_root, dst=dst_root) == []  # verify-only

    (dst_root / "d/orig.sigmf-data").unlink()
    (src_root / "store/d/orig.sigmf-data").write_bytes(b"IQ" * 1000)
    problems = fetch.fetch(m, src=src_root / "store", dst=dst_root)
    assert problems and not (dst_root / "d/orig.sigmf-data").exists()


def test_rds_reference_decoder_recovers_synthetic_pi_ps_and_stats(tmp_path):
    from hkpy.synth import generate

    out = tmp_path / "rds"
    generate("fm_broadcast_rds", 3, out, {"duration_s": 1.5}, "ci8")
    manifest = json.loads((out / "manifest.json").read_text())
    meta_path = out / manifest["recordings"][0]
    meta = sigmf.read_meta(meta_path)
    fs = meta["global"]["core:sample_rate"]
    raw = np.fromfile(sigmf.data_path(meta_path), dtype=np.int8).astype(np.float32).reshape(-1, 2)
    x = (raw[:, 0] + 1j * raw[:, 1]).astype(np.complex128)
    from scipy import signal
    x = np.convolve(x, signal.firwin(129, 140e3, fs=fs), mode="same")
    got = rds_ref.decode(x, fs)
    assert got["pi_hex"] == "C0DE"
    assert got["ps"] == "HACKRIFF" and not got["ps_dynamic"]
    assert got["pty"] == 10 and got["tp"] is True
    assert got["groups_decoded"] >= 8
    assert got["block_error_rate"] == 0.0
    assert abs(got["pilot_hz"] - 19000.0) < 0.5
    assert math.isclose(got["bitrate_bd_in_sample_clock"], got["pilot_hz"] / 16)


def committed_entries():
    manifest = fxlib.load_json(fxlib.MANIFEST)
    return manifest, [e for e in manifest["entries"] if e["status"] == "committed"]


def test_committed_manifest_verifies():
    manifest, committed = committed_entries()
    assert manifest["version"] == fxlib.MANIFEST_VERSION
    pointers = [e["path"] for e in committed if fxlib.is_lfs_pointer(fxlib.FIXTURES / e["path"])]
    if pointers:
        pytest.skip(f"Git LFS objects not pulled: {pointers[:2]}")
    problems, _ = verify.verify(fxlib.MANIFEST)
    assert problems == []


def test_committed_hackrf_fixtures_carry_provenance_and_truth():
    _, committed = committed_entries()
    metas = [fxlib.FIXTURES / e["path"] for e in committed if e.get("kind") == "sigmf-meta"]
    for path in metas:
        meta = sigmf.read_meta(path)
        g = meta["global"]
        assert g["core:license"] == "project-owned capture"
        assert any(uc in g["core:description"] for uc in ("SIGNAL-", "AWARE-", "SPACE-"))
        assert all("hackriff:clip_count" in c for c in meta["captures"])
        roles = {a[sigmf.TRUTH_KEY]["role"] for a in meta["annotations"] if sigmf.TRUTH_KEY in a}
        assert "scenario" in roles and roles <= annotate.ROLES
        for a in meta["annotations"]:
            t = a[sigmf.TRUTH_KEY]
            assert "payload_hex" not in t and "bits_hex" not in t
