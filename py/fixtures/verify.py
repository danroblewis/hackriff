"""Check fixtures against ``fixtures/manifest.json`` (sizes, sha256, LFS pointers resolved).

    uv run --project py python py/fixtures/verify.py [--external] [--store DIR] [--manifest PATH]

Committed entries must exist, match ``size_bytes`` and ``sha256``, and must not be unresolved
Git LFS pointer files (run ``git lfs pull``). Committed ``.sigmf-data`` must be within the size cap.
With ``--external``, originals in the store (``$HACKRIFF_FIXTURE_STORE``, default
fixtures/store) are checked too; a missing store file is reported, not fatal, unless
``--require-external``. Blocked entries need a ``reason``. Exit status 1 on any problem.

Manifest v2 (fixtures/README.md): ``{"version": 2, "store": "fixtures/store", "entries": [...]}``,
entry paths relative to ``fixtures/``; external ones start with ``store/``.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Any

import fxlib

STATUSES = frozenset({"committed", "external", "blocked"})


def resolve(entry: dict[str, Any], fixtures_root: Path, store: Path) -> Path:
    path = entry["path"]
    if path.startswith("store/"):
        return store / path[len("store/"):]
    return fixtures_root / path


def verify(manifest_path: Path = fxlib.MANIFEST, fixtures_root: Path | None = None,
           store: Path | None = None, external: bool = False,
           require_external: bool = False) -> tuple[list[str], list[str]]:
    """Returns (problems, notes)."""
    manifest_path = Path(manifest_path)
    fixtures_root = Path(fixtures_root) if fixtures_root else manifest_path.parent
    store = Path(store) if store else fxlib.store_dir()
    manifest = fxlib.load_json(manifest_path)
    problems: list[str] = []
    notes: list[str] = []
    if manifest.get("version") != fxlib.MANIFEST_VERSION:
        problems.append(f"manifest version {manifest.get('version')!r} != {fxlib.MANIFEST_VERSION}")
    names = set()
    for i, e in enumerate(manifest.get("entries", [])):
        name = e.get("name", f"#{i}")
        where = f"{name} ({e.get('path', '-')})"
        if (name, e.get("path")) in names:
            problems.append(f"{where}: duplicate entry")
        names.add((name, e.get("path")))
        status = e.get("status")
        if status not in STATUSES:
            problems.append(f"{where}: status {status!r} not in {sorted(STATUSES)}")
            continue
        if status == "blocked":
            if not e.get("reason"):
                problems.append(f"{where}: blocked without reason")
            continue
        for key in ("path", "size_bytes", "sha256"):
            if key not in e:
                problems.append(f"{where}: missing {key}")
        if any(k not in e for k in ("path", "size_bytes", "sha256")):
            continue
        is_external = status == "external"
        if is_external != e["path"].startswith("store/"):
            problems.append(f"{where}: {status} entry path must {'' if is_external else 'not '}start with store/")
            continue
        if is_external and not external:
            continue
        path = resolve(e, fixtures_root, store)
        if not path.exists():
            msg = f"{where}: missing at {path}"
            (problems if (not is_external or require_external) else notes).append(msg)
            continue
        if fxlib.is_lfs_pointer(path):
            problems.append(f"{where}: unresolved Git LFS pointer (run `git lfs pull`)")
            continue
        size = path.stat().st_size
        if size != e["size_bytes"]:
            problems.append(f"{where}: size {size} != manifest {e['size_bytes']}")
            continue
        if not is_external and path.suffix == ".sigmf-data" and size > fxlib.MAX_COMMITTED_BYTES:
            problems.append(f"{where}: {size} bytes exceeds the committed cap {fxlib.MAX_COMMITTED_BYTES}")
        digest = fxlib.sha256_file(path)
        if digest != e["sha256"]:
            problems.append(f"{where}: sha256 {digest} != manifest {e['sha256']}")
    return problems, notes


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--manifest", default=str(fxlib.MANIFEST))
    ap.add_argument("--store", help="external store directory (default $HACKRIFF_FIXTURE_STORE or fixtures/store)")
    ap.add_argument("--external", action="store_true", help="also verify external originals")
    ap.add_argument("--require-external", action="store_true", help="missing external files are errors")
    args = ap.parse_args(argv)
    problems, notes = verify(Path(args.manifest), store=fxlib.store_dir(args.store),
                             external=args.external or args.require_external,
                             require_external=args.require_external)
    for n in notes:
        print("note:", n)
    for p in problems:
        print("FAIL:", p)
    print(f"{'FAILED' if problems else 'ok'}: {len(problems)} problem(s), {len(notes)} note(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
