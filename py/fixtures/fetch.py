"""Copy or verify external originals listed in ``fixtures/manifest.json``. No network.

    uv run --project py python py/fixtures/fetch.py [--from DIR] [--to DIR] [NAME ...]

``--from`` is where the originals are now (a mounted disk, another clone's store); default
``$HACKRIFF_FIXTURE_STORE`` or fixtures/store. ``--to`` is the store to fill; default
fixtures/store. Each ``external`` entry (optionally only NAMEs) is checked by size and sha256 at
``--to``; if absent or wrong it is copied from ``--from`` after the source checksum matches.
With ``--from`` == ``--to`` this only verifies. Exit status 1 if anything is missing or corrupt.
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path
from typing import Any

import fxlib


def _ok(path: Path, e: dict[str, Any]) -> bool:
    return path.exists() and path.stat().st_size == e["size_bytes"] and fxlib.sha256_file(path) == e["sha256"]


def fetch(manifest_path: Path = fxlib.MANIFEST, src: Path | None = None, dst: Path | None = None,
          names: list[str] | None = None) -> list[str]:
    """Returns problems (empty on success)."""
    manifest = fxlib.load_json(Path(manifest_path))
    src = Path(src) if src else fxlib.store_dir()
    dst = Path(dst) if dst else (fxlib.FIXTURES / "store")
    problems = []
    for e in manifest["entries"]:
        if e.get("status") != "external" or (names and e["name"] not in names):
            continue
        rel = e["path"][len("store/"):]
        target, source = dst / rel, src / rel
        if _ok(target, e):
            print(f"ok      {rel}")
            continue
        if source.resolve() == target.resolve():
            problems.append(f"{rel}: missing or checksum mismatch in {dst}")
            continue
        if not _ok(source, e):
            problems.append(f"{rel}: not available with matching checksum in {src}")
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        tmp = target.with_name(target.name + ".partial")
        shutil.copyfile(source, tmp)
        if not _ok(tmp, e):
            tmp.unlink(missing_ok=True)
            problems.append(f"{rel}: copy failed checksum")
            continue
        tmp.replace(target)
        print(f"copied  {rel}")
    return problems


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("names", nargs="*")
    ap.add_argument("--manifest", default=str(fxlib.MANIFEST))
    ap.add_argument("--from", dest="src")
    ap.add_argument("--to", dest="dst")
    args = ap.parse_args(argv)
    problems = fetch(Path(args.manifest), fxlib.store_dir(args.src),
                     Path(args.dst) if args.dst else None, args.names or None)
    for p in problems:
        print("FAIL:", p)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
