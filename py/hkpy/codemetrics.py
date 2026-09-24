"""Code metrics over committed main, for the dashboard's /metrics page (user, 2026-09-24).

Read-only. Everything is computed from `git ls-tree` / `git cat-file` / `git log` at ONE ref - main,
or while a batch gates, the bulk marker's base (taskorder.committed_ref): never the provisional tip,
never a working tree. The gate's JUnit files and merge-runner.log supply test counts, test time and
the last lint's warnings. Each method's caveat travels with its numbers (`CAVEATS`), and the page
prints them.

Line counting (the user's 09-24 ad-hoc baseline, and where this differs from it):
  * non-blank, non-comment lines; comments are whole lines starting with `//` or `/* ... */`
    (Rust, TS, JS, CSS, WGSL) or `#` (Python, Shell). Trailing comments, doc comments (`///` counts
    as a comment) and Python docstrings (counted as code) are the known skews.
  * excluded: docs/ fixtures/ tools/ spikes/ (throwaway, per CLAUDE.md), lockfiles, dist/,
    node_modules/ - and any language not listed in LANG (TOML, YAML, JSON, HTML, Markdown).
  * test = Rust under tests/ or benches/, a `tests.rs` / `*_tests.rs` / `test.rs` module file,
    and in any other Rust file everything from the first `#[cfg(test)]` to the end of the file;
    Python under a tests/ directory or test_*.py; TS/JS under ui/test/, ui/e2e/ or *.test.*.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from collections import Counter, defaultdict
from datetime import datetime

LANG = {".rs": "Rust", ".py": "Python", ".ts": "TypeScript", ".tsx": "TypeScript", ".js": "JS", ".mjs": "JS",
        ".cjs": "JS", ".sh": "Shell", ".css": "CSS", ".wgsl": "WGSL"}
EXCLUDED_DIRS = ("docs/", "fixtures/", "tools/", "spikes/")
_LOCK = re.compile(r"(^|/)(Cargo\.lock|package-lock\.json|uv\.lock|pnpm-lock\.yaml|yarn\.lock)$")
_HASH_COMMENT = ("Python", "Shell")
SAMPLES = "metrics.jsonl"

CAVEATS = {
    "lines": "Non-blank, non-comment lines by whole-line comment markers: trailing comments count as code, "
             "Python docstrings count as code, Rust /// and //! count as comments. Excluded: docs/ fixtures/ "
             "tools/ spikes/, lockfiles, dist/, node_modules/, and every language not in the table. "
             "Test = Rust tests/ benches/ and tests.rs/*_tests.rs modules plus everything from a file's "
             "first #[cfg(test)] to its end (a product item after a test module is miscounted as test); "
             "Python tests/ and test_*.py; TS/JS ui/test/, ui/e2e/, *.test.*.",
    "churn": "git log --numstat --no-merges over the same ref and exclusions: lines added/removed per commit, "
             "so a line rewritten twice counts twice, a rename counts as delete + add, and a rebuilt -rl "
             "branch's copies count once each (they are different commits).",
    "tests": "From the newest JUnit files the gate kept ($HACKRIFF_OPS/junit/<run>/*-test-*.xml and "
             "*-acceptance-*.xml): one gate's run, not an average. Seconds are the SUM of per-test times - "
             "tests run in parallel, so this is test-seconds, not wall time - and the timing tier is not in "
             "any gate run. Per 1k lines divides by the crate's product + test lines. Only the Rust suites (nextest) "
             "write JUnit: pytest, the UI unit tests and the browser e2e tier are not in this table.",
    "outliers": "Longest Rust functions are a brace-depth proxy: from a line with `fn name` to where its braces "
                "balance, ignoring braces inside strings and chars. Nested fns and closures are inside their "
                "parent. Good for ranking, not for exact counts.",
    "hygiene": "#[ignore] is grouped by its reason string: hardware-in-the-loop tests (\"needs a HackRF\") and release-build "
               "benches are ignored by design and run with --ignored; one with no reason is the one to look at. "
               "Counts of regex matches in non-comment Rust lines (unsafe) and in any line (TODO/FIXME/XXX), per "
               "crate/area. Clippy warnings = `warning:` lines in the last `just lint` section of "
               "merge-runner.log (the gate runs clippy with -D warnings, so a green gate reads 0). #[ignore] "
               "is counted in Rust source; quarantine = entries in ui/e2e/quarantine.json. Both should be 0.",
    "trends": "One sample per day in $HACKRIFF_OPS/metrics.jsonl, taken by the first build after midnight - "
              "a day with no landing and no dashboard has no sample.",
}


def _git(repo: str, *args: str, timeout: int = 60) -> str:
    return subprocess.run(["git", "-C", repo, *args], capture_output=True, text=True, timeout=timeout, check=True).stdout


def included(path: str) -> bool:
    return (bool(path) and not path.startswith(EXCLUDED_DIRS) and "/dist/" not in path and not path.startswith("dist/")
            and "node_modules/" not in path and not _LOCK.search(path) and os.path.splitext(path)[1] in LANG)


def lang_of(path: str) -> str:
    return LANG[os.path.splitext(path)[1]]


def area_of(path: str) -> str:
    parts = path.split("/")
    if parts[0] == "crates" and len(parts) > 2:
        return parts[1]
    if parts[0] == "tests":
        return "tests (hk-e2e)"
    if parts[0] == "ui":
        return "ui/src/" + parts[2] if len(parts) > 3 and parts[1] == "src" else "ui/" + (parts[1] if len(parts) > 2 else "")
    if parts[0] == "py":
        return "py/" + (parts[1] if len(parts) > 2 else "")
    return parts[0] if len(parts) > 1 else "(root)"


def is_test_path(path: str, lang: str) -> bool:
    if lang == "Rust":
        return bool(re.search(r"(^|/)(tests|benches)/", path) or re.search(r"(^|/|_)tests?\.rs$", path))
    if lang == "Python":
        return bool(re.search(r"(^|/)tests/", path) or re.search(r"(^|/)test_[^/]*\.py$", path))
    if lang in ("TypeScript", "JS"):
        return path.startswith(("ui/test/", "ui/e2e/")) or ".test." in path
    return False


def code_lines(lines: list[str], lang: str) -> list[int]:
    """Indices of the counted (non-blank, non-comment) lines."""
    out, block = [], False
    for i, ln in enumerate(lines):
        s = ln.strip()
        if not s:
            continue
        if lang in _HASH_COMMENT:
            if s.startswith("#"):          # comments and the shebang
                continue
        else:
            if block:
                if "*/" in s:
                    block = False
                continue
            if s.startswith("//"):
                continue
            if s.startswith("/*"):
                block = "*/" not in s
                continue
        out.append(i)
    return out


def classify(path: str, text: str) -> dict:
    lang = lang_of(path)
    lines = text.splitlines()
    idx = code_lines(lines, lang)
    if is_test_path(path, lang):
        cut = 0
    elif lang == "Rust":
        cut = next((i for i, ln in enumerate(lines) if re.match(r"\s*#\[cfg\(test\)\]", ln)), len(lines))
    else:
        cut = len(lines)
    product = sum(1 for i in idx if i < cut)
    return {"path": path, "lang": lang, "area": area_of(path), "product": product, "test": len(idx) - product,
            "lines": lines, "code_idx": idx}


def read_tree(repo: str, ref: str) -> list[dict]:
    names = [p for p in _git(repo, "ls-tree", "-r", "-z", "--name-only", ref).split("\0") if included(p)]
    proc = subprocess.Popen(["git", "-C", repo, "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    files = []
    try:
        for path in names:
            proc.stdin.write(f"{ref}:{path}\n".encode())
            proc.stdin.flush()
            hdr = proc.stdout.readline().split()
            if len(hdr) < 3 or hdr[1] != b"blob":
                continue
            data = proc.stdout.read(int(hdr[2]))
            proc.stdout.read(1)
            files.append(classify(path, data.decode("utf-8", "replace")))
    finally:
        proc.stdin.close()
        proc.wait(timeout=30)
    return files


def lines_table(files: list[dict]) -> dict:
    by_lang: dict = defaultdict(Counter)
    by_area: dict = defaultdict(Counter)
    for f in files:
        for key, table in ((f["lang"], by_lang), (f["area"], by_area)):
            table[key]["product"] += f["product"]
            table[key]["test"] += f["test"]
            table[key]["files"] += 1
    total = Counter()
    for c in by_lang.values():
        total.update(c)
    rows = lambda t: sorted(({"name": k, **v} for k, v in t.items()), key=lambda r: -(r["product"] + r["test"]))  # noqa: E731
    return {"total": dict(total), "by_lang": rows(by_lang), "by_area": rows(by_area)}


def churn(repo: str, ref: str, now: float) -> dict:
    """Added/removed per area over 24 h / 7 d / 30 d, and the 15 hottest files over 7 d."""
    text = _git(repo, "log", ref, "--no-merges", f"--since={int(now - 30 * 86400)}", "--numstat", "--format=@%ct", timeout=120)
    windows = {"24h": 86400, "7d": 7 * 86400, "30d": 30 * 86400}
    per: dict = {w: defaultdict(Counter) for w in windows}
    hot = Counter()
    ts = 0
    for ln in text.splitlines():
        if ln.startswith("@"):
            ts = int(ln[1:])
            continue
        parts = ln.split("\t")
        if len(parts) != 3 or parts[0] == "-":
            continue
        path = parts[2].split(" => ")[-1].replace("}", "").replace("{", "")
        if not included(path):
            continue
        a, r = int(parts[0]), int(parts[1])
        for w, span in windows.items():
            if now - ts <= span:
                per[w][area_of(path)]["added"] += a
                per[w][area_of(path)]["removed"] += r
        if now - ts <= 7 * 86400:
            hot[path] += a + r
    return {"by_area": {w: sorted(({"name": k, **v} for k, v in t.items()), key=lambda r: -(r["added"] + r["removed"]))
                        for w, t in per.items()},
            "totals": {w: {"added": sum(v["added"] for v in t.values()), "removed": sum(v["removed"] for v in t.values())}
                       for w, t in per.items()},
            "hottest_7d": [{"path": p, "changed": n} for p, n in hot.most_common(15)]}


def junit_tests(ops: str) -> dict:
    """{area: {tests, seconds}} from the newest kept JUnit run that has a test suite."""
    root = os.path.join(ops, "junit")
    try:
        runs = sorted((os.path.join(root, d) for d in os.listdir(root)), key=os.path.getmtime, reverse=True)
    except OSError:
        return {"run": None, "by_area": {}}
    for run in runs:
        try:
            xmls = [x for x in os.listdir(run) if x.endswith(".xml") and ("-test-" in x or "-acceptance" in x)]
        except OSError:
            continue
        if not any("-test-" in x for x in xmls):
            continue
        per: dict = defaultdict(Counter)
        for x in xmls:
            try:
                body = open(os.path.join(run, x), encoding="utf-8", errors="replace").read()
            except OSError:
                continue
            for m in re.finditer(r'<testcase [^>]*classname="([^"]+)"[^>]*time="([\d.]+)"', body):
                crate = m.group(1).split("::")[0]
                area = "tests (hk-e2e)" if crate == "hk-e2e" else crate
                per[area]["tests"] += 1
                per[area]["seconds"] += float(m.group(2))
        return {"run": os.path.basename(run), "at": os.path.getmtime(run),
                "by_area": {k: {"tests": v["tests"], "seconds": round(v["seconds"], 1)} for k, v in per.items()}}
    return {"run": None, "by_area": {}}


_FN = re.compile(r"\bfn\s+([A-Za-z_]\w*)")


def longest_fns(files: list[dict], top: int = 20) -> list[dict]:
    """Brace-depth proxy over Rust files."""
    out = []
    for f in files:
        if f["lang"] != "Rust":
            continue
        lines = f["lines"]
        i = 0
        while i < len(lines):
            m = _FN.search(lines[i])
            if not m or lines[i].lstrip().startswith("//"):
                i += 1
                continue
            depth, opened, j = 0, False, i
            while j < len(lines):
                s = re.sub(r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])\'', "", lines[j].split("//")[0])
                depth += s.count("{") - s.count("}")
                opened = opened or "{" in s
                if (opened and depth <= 0) or (not opened and s.rstrip().endswith(";")):
                    break
                j += 1
            if opened:
                out.append({"fn": m.group(1), "path": f["path"], "line": i + 1, "lines": j - i + 1,
                            "test": i >= _test_cut(f)})
            i += 1
    out.sort(key=lambda r: -r["lines"])
    return out[:top]


def _test_cut(f: dict) -> int:
    if is_test_path(f["path"], f["lang"]):
        return 0
    return next((i for i, ln in enumerate(f["lines"]) if re.match(r"\s*#\[cfg\(test\)\]", ln)), len(f["lines"]))


def hygiene(files: list[dict], repo: str, ref: str, ops: str) -> dict:
    per: dict = defaultdict(Counter)
    ignores = []
    for f in files:
        code = set(f["code_idx"])
        for i, ln in enumerate(f["lines"]):
            if re.search(r"\b(TODO|FIXME|XXX)\b", ln):
                per[f["area"]]["todo"] += 1
            if f["lang"] == "Rust" and i in code:
                if re.search(r"\bunsafe\s*(\{|fn\b|impl\b)", ln):
                    per[f["area"]]["unsafe"] += 1
                m = re.match(r'\s*#\[ignore(?:\s*=\s*"([^"]*)")?', ln)
                if m:
                    ignores.append({"at": f"{f['path']}:{i + 1}", "why": m.group(1) or ""})
    try:
        quarantine = json.loads(_git(repo, "show", f"{ref}:ui/e2e/quarantine.json"))
        quarantined = len(quarantine) if isinstance(quarantine, (list, dict)) else None
    except Exception:
        quarantined = None
    reasons = Counter(x["why"] or "(no reason given)" for x in ignores)
    return {"by_area": sorted(({"name": k, **v} for k, v in per.items() if v), key=lambda r: -(r.get("unsafe", 0) * 100 + r.get("todo", 0))),
            "unsafe": sum(v["unsafe"] for v in per.values()), "todo": sum(v["todo"] for v in per.values()),
            "ignored": ignores, "ignored_by_reason": reasons.most_common(), "quarantined": quarantined,
            "clippy": clippy_warnings(ops)}


def clippy_warnings(ops: str) -> dict:
    """`warning:` lines in the last `just lint` section of merge-runner.log (tail 20 MB)."""
    path = os.path.join(ops, "merge-runner.log")
    try:
        with open(path, "rb") as fh:
            fh.seek(max(0, os.path.getsize(path) - 20_000_000))
            text = fh.read().decode("utf-8", "replace")
    except OSError:
        return {"warnings": None, "at": None}
    end = text.rfind("gate: just lint took")
    if end < 0:
        return {"warnings": None, "at": None}
    start = text.rfind("gate: running just lint", 0, end)
    section = text[start if start >= 0 else max(0, end - 2_000_000):end]
    n = sum(1 for ln in section.splitlines() if ln.startswith("warning:") and "generated" not in ln)
    tail = text[end:end + 80].splitlines()[0]
    return {"warnings": n, "line": tail}


def build(repo: str, ops: str, now: float | None = None) -> dict:
    from hkpy import taskorder
    now = now or time.time()
    ref = taskorder.committed_ref(repo, ops)
    sha = _git(repo, "rev-parse", ref).strip()
    t0 = time.time()
    files = read_tree(repo, sha)
    lines = lines_table(files)
    tests = junit_tests(ops)
    area_lines = {r["name"]: r["product"] + r["test"] for r in lines["by_area"]}
    cost = []
    for area, v in tests["by_area"].items():
        n = area_lines.get(area) or 0
        cost.append({"name": area, "tests": v["tests"], "seconds": v["seconds"], "lines": n,
                     "s_per_1k": round(v["seconds"] / (n / 1000), 1) if n else None})
    cost.sort(key=lambda r: -(r["s_per_1k"] or 0))
    largest = sorted(({"path": f["path"], "lines": f["product"] + f["test"], "product": f["product"], "test": f["test"]}
                      for f in files), key=lambda r: -r["lines"])[:20]
    out = {"ref": ref, "sha": sha, "built": now, "files": len(files), "lines": lines,
           "churn": churn(repo, sha, now), "tests": {"run": tests.get("run"), "at": tests.get("at"), "by_area": cost},
           "largest": largest, "longest_fns": longest_fns(files), "hygiene": hygiene(files, repo, sha, ops),
           "caveats": CAVEATS}
    try:
        from hkpy import codearch
        out["arch"] = codearch.analyse(files, repo, sha, now)
    except Exception as e:                      # a part that fails is named, the rest still ships
        out["arch"] = {"error": f"{type(e).__name__}: {e}"}
    try:
        from hkpy import codecomplex
        out["complexity"] = codecomplex.analyse(files, repo, sha, ops, now)
    except Exception as e:
        out["complexity"] = {"error": f"{type(e).__name__}: {e}"}
    out["build_s"] = round(time.time() - t0, 1)
    out["trend"] = sample(ops, out, now)
    return out


def sample(ops: str, m: dict, now: float) -> list[dict]:
    """Append today's sample once; return every sample (oldest first)."""
    path = os.path.join(ops, SAMPLES)
    rows = []
    try:
        for ln in open(path, encoding="utf-8"):
            try:
                rows.append(json.loads(ln))
            except ValueError:
                pass
    except OSError:
        pass
    day = datetime.fromtimestamp(now).strftime("%Y-%m-%d")
    # a dashboard PREVIEW (ops/preview-dashboard.sh) runs a branch's code: it never writes the trend
    if not os.environ.get("HK_METRICS_NO_SAMPLE") and not any(r.get("date") == day for r in rows):
        rec = {"date": day, "ts": now, "sha": m["sha"], "product": m["lines"]["total"].get("product", 0),
               "test": m["lines"]["total"].get("test", 0),
               "by_lang": {r["name"]: [r["product"], r["test"]] for r in m["lines"]["by_lang"]},
               "churn_24h": m["churn"]["totals"]["24h"]}
        a = m.get("arch") or {}
        if a.get("per_crate"):
            coup = {r["name"]: r for r in a.get("coupling", [])}
            rec["crates"] = {p["name"]: {"lines": p["product_lines"], "pub": p["pub_items"], "undoc": p["undocumented"],
                                         "unwrap": p["idioms"].get("unwrap()/expect()", 0), "unsafe": p["idioms"].get("unsafe", 0),
                                         "zstd": p["zstd_ratio"], "I": coup.get(p["name"], {}).get("instability"),
                                         "D": coup.get(p["name"], {}).get("distance")} for p in a["per_crate"]}
            rules = a.get("rules") or {}
            cx = m.get("complexity") or {}
            for a in cx.get("areas") or []:
                if a["name"] in rec["crates"]:
                    rec["crates"][a["name"]].update(cog90=a["cognitive_p90"], cogmax=a["cognitive_max"], mi=a["mi"],
                                                    fns=a["functions"])
            rec["rules"] = {"layer": len(rules.get("layer_violations") or []), "gpl": (rules.get("gpl") or {}).get("violations"),
                            "ui_dsp_suspects": len(rules.get("ui_dsp_suspects") or [])}
        rows.append(rec)
        try:
            with open(path, "a", encoding="utf-8") as fh:
                fh.write(json.dumps(rec) + "\n")
        except OSError:
            pass
    return rows[-90:]


def main() -> int:
    ops = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
    repo = os.environ.get("HACKRIFF_REPO") or "/Users/daniellewis/hackriff"
    m = build(repo, ops)
    t = m["lines"]["total"]
    print(f"code metrics at {m['ref']} ({m['sha'][:8]}), {m['files']} files, built in {m['build_s']} s")
    print(f"product {t.get('product', 0):,} / test {t.get('test', 0):,}")
    for r in m["lines"]["by_lang"]:
        print(f"  {r['name']:<11} {r['product']:>8,} / {r['test']:>8,}  ({r['files']} files)")
    c = m["churn"]["totals"]
    print("churn +/-: " + "  ".join(f"{w} +{v['added']:,}/-{v['removed']:,}" for w, v in c.items()))
    h = m["hygiene"]
    print(f"hygiene: unsafe {h['unsafe']} · TODO/FIXME/XXX {h['todo']} · #[ignore] {len(h['ignored'])} · "
          f"quarantined {h['quarantined']} · clippy warnings {h['clippy']['warnings']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
