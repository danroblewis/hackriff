"""/metrics phase 2, part B (user, 2026-09-24): complexity and hotspots.

rust-code-analysis-cli (one CLI for Rust, TypeScript, JavaScript and Python) over the committed tree
- extracted from git at the sha into a temporary directory, never a working tree - gives per function
and per file: cyclomatic, cognitive, Halstead and the maintainability index. Hotspots are Tornhill's:
a file's 30-day churn times its complexity, so the list names code that is both hard and changing.
The CLI lives in $HACKRIFF_OPS/tools/bin (cargo install rust-code-analysis-cli --root there); when it
is missing the section says so instead of guessing.
"""

from __future__ import annotations

import json
import os
import shutil
import statistics
import subprocess
import tempfile
from collections import Counter, defaultdict

CAVEATS = {
    "complexity": "rust-code-analysis-cli 0.0.25 over the committed tree. Cyclomatic = McCabe (1 + decision "
                  "points); cognitive = SonarSource's nesting-weighted count, the better 'hard to read' signal; "
                  "Halstead effort from operator/operand counts; MI = the Visual Studio 0-100 maintainability "
                  "index (higher is better; it is per FILE and falls with length, so long files read low). Closures count inside their function; a test function is one "
                  "under tests/ or after the file's first #[cfg(test)]. Python and TS are measured by the same "
                  "tool's grammars, so compare within a language, not across.",
    "hotspots": "Tornhill hotspot = 30-day churn (lines added + removed, git log --numstat --no-merges) x the "
                "file's summed cognitive complexity. It ranks where hard code is also changing - a place to "
                "look, not a defect list. Test files are excluded.",
}
BUCKETS = ((0, 4), (5, 9), (10, 19), (20, 49), (50, 10 ** 9))


def tool(ops: str) -> str | None:
    p = os.path.join(ops, "tools", "bin", "rust-code-analysis-cli")
    return p if os.path.exists(p) else shutil.which("rust-code-analysis-cli")


def _decode_stream(text: str):
    dec, i, n = json.JSONDecoder(), 0, len(text)
    while i < n:
        while i < n and text[i].isspace():
            i += 1
        if i >= n:
            break
        obj, i = dec.raw_decode(text, i)
        yield obj


def _functions(space: dict, out: list):
    for s in space.get("spaces") or []:
        if s.get("kind") == "function":
            m = s.get("metrics") or {}
            out.append({"name": s.get("name") or "?", "start": s.get("start_line"), "end": s.get("end_line"),
                        "cyclomatic": (m.get("cyclomatic") or {}).get("sum"), "cognitive": (m.get("cognitive") or {}).get("sum"),
                        "effort": round((m.get("halstead") or {}).get("effort") or 0),
                        "mi": round((m.get("mi") or {}).get("mi_visual_studio") or 0, 1)})
        _functions(s, out)


def run(exe: str, repo: str, sha: str, paths: list[str]) -> dict[str, dict]:
    """{repo path: {"file": metrics, "functions": [...]}} for `paths` at `sha`."""
    tmp = tempfile.mkdtemp(prefix="hk-rca-")
    try:
        arch = subprocess.Popen(["git", "-C", repo, "archive", sha, "--", *sorted(paths)], stdout=subprocess.PIPE)
        subprocess.run(["tar", "-x", "-C", tmp], stdin=arch.stdout, check=True)
        arch.wait(timeout=120)
        keep = set(paths)
        out = subprocess.run([exe, "-m", "-O", "json", "-p", tmp, "-j", "4"], capture_output=True, text=True,
                             timeout=600).stdout
        res = {}
        for obj in _decode_stream(out):
            rel = os.path.relpath(obj.get("name", ""), tmp)
            if rel not in keep:
                continue
            m = obj.get("metrics") or {}
            fns = []
            _functions(obj, fns)
            res[rel] = {"cyclomatic": (m.get("cyclomatic") or {}).get("sum"), "cognitive": (m.get("cognitive") or {}).get("sum"),
                        "effort": round((m.get("halstead") or {}).get("effort") or 0),
                        "mi": round((m.get("mi") or {}).get("mi_visual_studio") or 0, 1),
                        "sloc": (m.get("loc") or {}).get("sloc"), "functions": fns}
        return res
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def churn_30d(repo: str, sha: str, now: float) -> Counter:
    text = subprocess.run(["git", "-C", repo, "log", sha, "--no-merges", f"--since={int(now - 30 * 86400)}", "--numstat",
                           "--format="], capture_output=True, text=True, timeout=120).stdout
    c = Counter()
    for ln in text.splitlines():
        parts = ln.split("\t")
        if len(parts) == 3 and parts[0] != "-":
            c[parts[2].split(" => ")[-1].replace("{", "").replace("}", "")] += int(parts[0]) + int(parts[1])
    return c


def _pct(xs: list[float], q: float):
    if not xs:
        return None
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(q * len(xs)))]


def analyse(files: list[dict], repo: str, sha: str, ops: str, now: float) -> dict:
    from hkpy.codemetrics import _test_cut, is_test_path
    exe = tool(ops)
    if not exe:
        return {"error": "rust-code-analysis-cli not installed (cargo install rust-code-analysis-cli --root $HACKRIFF_OPS/tools)"}
    by_path = {f["path"]: f for f in files}
    res = run(exe, repo, sha, list(by_path))
    per_area: dict = defaultdict(lambda: {"fns": [], "mi": [], "cog_sum": 0, "sloc": 0})
    all_fns, dist = [], defaultdict(Counter)
    for path, r in res.items():
        f = by_path[path]
        cut = _test_cut(f)
        test_file = is_test_path(path, f["lang"])
        a = per_area[f["area"]]
        a["lang"] = f["lang"]
        for fn in r["functions"]:
            fn = dict(fn, path=path, lang=f["lang"], test=test_file or (fn["start"] or 0) - 1 >= cut)
            all_fns.append(fn)
            if not fn["test"]:
                a["fns"].append(fn)
                cog = fn["cognitive"] or 0
                dist[f["lang"]][next(f"{lo}-{hi}" if hi < 10 ** 9 else f"{lo}+" for lo, hi in BUCKETS if lo <= cog <= hi)] += 1
        if not test_file:
            a["cog_sum"] += r["cognitive"] or 0
            if r["sloc"]:
                a["mi"].append((r["mi"], r["sloc"]))
                a["sloc"] += r["sloc"]
    areas = []
    for name, a in per_area.items():
        cogs = [fn["cognitive"] or 0 for fn in a["fns"]]
        cycs = [fn["cyclomatic"] or 0 for fn in a["fns"]]
        mi = sum(m * s for m, s in a["mi"]) / sum(s for _, s in a["mi"]) if a["mi"] else None
        areas.append({"name": name, "lang": a.get("lang"), "functions": len(a["fns"]),
                      "cognitive_p50": _pct(cogs, .5), "cognitive_p90": _pct(cogs, .9), "cognitive_max": max(cogs) if cogs else None,
                      "cyclomatic_p90": _pct(cycs, .9), "cyclomatic_max": max(cycs) if cycs else None,
                      "mi": round(mi, 1) if mi is not None else None, "cognitive_sum": a["cog_sum"]})
    areas.sort(key=lambda r: -(r["cognitive_sum"] or 0))
    prod = [fn for fn in all_fns if not fn["test"]]
    worst = sorted(prod, key=lambda fn: -(fn["cognitive"] or 0))[:20]
    ch = churn_30d(repo, sha, now)
    hot = []
    for path, r in res.items():
        if is_test_path(path, by_path[path]["lang"]) or not ch.get(path):
            continue
        hot.append({"path": path, "churn_30d": ch[path], "cognitive": r["cognitive"] or 0, "mi": r["mi"],
                    "score": ch[path] * (r["cognitive"] or 0)})
    hot.sort(key=lambda h: -h["score"])
    return {"tool": exe, "files": len(res), "functions": len(all_fns), "product_functions": len(prod),
            "areas": areas, "worst": worst, "distribution": {k: dict(v) for k, v in dist.items()},
            "buckets": [f"{lo}-{hi}" if hi < 10 ** 9 else f"{lo}+" for lo, hi in BUCKETS],
            "hotspots": hot[:20],
            "cognitive_p90_all": _pct([fn["cognitive"] or 0 for fn in prod], .9),
            "median_mi": statistics.median([r["mi"] for r in res.values() if r["sloc"]]) if res else None,
            "caveats": CAVEATS}
