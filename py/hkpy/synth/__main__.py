"""CLI: ``uv run python -m hkpy.synth <scenario> --seed N --out DIR [--param k=v ...]``."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from hkpy.synth import SCENARIOS, ParamError, generate, scenario_defaults
from hkpy.synth.scene import SUPPORTED_DATATYPES


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(prog="python -m hkpy.synth",
                                 description="Generate labelled synthetic IQ as SigMF (hackriff T-023).")
    ap.add_argument("scenario", nargs="?", choices=sorted(SCENARIOS))
    ap.add_argument("--seed", type=int, default=0)
    ap.add_argument("--out", type=Path, help="output directory")
    ap.add_argument("--param", action="append", default=[], metavar="K=V",
                    help="override a parameter; lists are comma-separated; 'none' for null")
    ap.add_argument("--datatype", choices=SUPPORTED_DATATYPES, default="ci8")
    ap.add_argument("--list", action="store_true", help="list scenarios and their parameters")
    args = ap.parse_args(argv)

    if args.list:
        for name, spec in SCENARIOS.items():
            uc = ", ".join(spec.use_cases) or "building block"
            print(f"{name}  ({uc}): {spec.summary}")
            print("  " + json.dumps(scenario_defaults(name)))
        return 0
    if args.scenario is None or args.out is None:
        ap.error("scenario and --out are required (or use --list)")
    overrides = {}
    for item in args.param:
        key, sep, value = item.partition("=")
        if not sep:
            ap.error(f"--param expects K=V, got {item!r}")
        overrides[key.strip()] = value
    try:
        manifest = generate(args.scenario, args.seed, args.out, overrides, args.datatype)
    except ParamError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2
    print(manifest)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
