"""/metrics phase 2, part A (user, 2026-09-24): coupling, architecture rules, idioms, API, docs,
compression and the crate graph - everything answerable from the committed tree, the crates'
Cargo.toml and `cargo metadata`, with no new tool. hkpy.codemetrics.build() calls `analyse` with the
files it already read at the committed sha; each section's caveat is in CAVEATS and on the page.
Complexity and hotspots (rust-code-analysis) and duplication / import cycles (jscpd, madge) are later
parts, named as such on the page rather than approximated here.
"""

from __future__ import annotations

import json
import re
import subprocess
import tomllib
from collections import Counter, defaultdict

#: The user's layer order: a crate may depend only on crates BELOW it. Crates not in the list are
#: unranked - no edge to or from them is judged.
LAYERS = ("hk-model", "hk-core", "hk-dsp", "hk-detect", "hk-estimate", "hk-demod", "hk-store",
          "hk-pipeline", "hk-api", "hk-cli")
RANK = {c: i for i, c in enumerate(LAYERS)}
_PERMISSIVE = ("MIT", "Apache", "BSD", "ISC", "Zlib", "MPL", "Unlicense", "CC0", "BSL", "Unicode")

IDIOMS = {  # product Rust lines only
    "dyn Trait": r"\bdyn\s+[A-Z]",
    "generic fns/impls": r"\bfn\s+\w+\s*<|\bimpl\s*<",
    "Arc<Mutex|RwLock>": r"\bArc\s*<\s*(?:std::sync::|parking_lot::)?(?:Mutex|RwLock)\b",
    "static/OnceCell/lazy": r"^\s*(?:pub(?:\([^)]*\))?\s+)?static\s|\bOnce(?:Cell|Lock)\b|\bLazyLock\b|\blazy_static!",
    "mpsc": r"\bmpsc\b",
    "fn build(": r"\bfn\s+build\s*\(",
    "unsafe": r"\bunsafe\s*(?:\{|fn\b|impl\b)",
    "unwrap()/expect()": r"\.unwrap\(\)|\.expect\(",
}
_IDIOM_RE = {k: re.compile(v) for k, v in IDIOMS.items()}
_PUB = re.compile(r"^\s*pub\s+(?:async\s+|const\s+(?=fn)|unsafe\s+|extern\s+\"C\"\s+)*(fn|struct|enum|trait|type|const|static|mod|union|use|macro)\b")
_UI_DSP = re.compile(r"\b(i?fft\w*|FFT\w*|hann\w*|hamming|blackman\w*|kaiser\w*|biquad\w*|goertzel\w*|demodulat\w*|"
                     r"decimat\w*|resampl\w*|convol\w*|fir|iir|windowFunction)\b")

CAVEATS = {
    "coupling": "Crate edges from each crate's Cargo.toml at the committed sha ([dependencies] and target "
                "deps; dev-dependencies counted separately). Ce = workspace crates it depends on, Ca = those "
                "depending on it, instability I = Ce/(Ca+Ce). Abstractness A = pub traits / pub type-defining "
                "items (struct, enum, trait, union, type) - a trait-share proxy, not Martin's abstract-class "
                "ratio. Distance D = |A + I - 1|. Edge weight = occurrences of `crate_name::` in the depender.",
    "rules": "Layer order (user): hk-model -> hk-core -> hk-dsp -> hk-detect -> hk-estimate -> hk-demod -> "
             "hk-store -> hk-pipeline -> hk-api -> hk-cli; a normal dependency on a HIGHER crate is a violation; "
             "crates outside the list are unranked and never judged. GPL: every package in a workspace crate's "
             "normal-dependency closure from `cargo metadata` (main's checkout, so it can lag the committed sha "
             "by a batch) whose licence has no permissive alternative. DSP in ui/src: a KEYWORD proxy (fft, "
             "window functions, filters, demod, resample...) in product TS - each hit is a suspect to read, "
             "not a proven violation.",
    "idioms": "Regex counts over PRODUCT Rust lines (the test split of the Lines table), comments excluded. "
              "Generics = generic fn signatures + generic impls; statics include `pub static` and OnceCell/"
              "OnceLock/LazyLock/lazy_static. unwrap()/expect() counts calls, not panics reachable in practice.",
    "api": "Pub items = product lines starting `pub fn|struct|enum|trait|type|const|static|mod|union|use|macro` "
           "(pub(crate) excluded; fields and methods of impls count as fns). Churn = such lines added/removed "
           "in `git log -p --no-merges` over 7 days in product .rs files. Missing docs = pub items (not `pub "
           "use`/`pub mod`) whose preceding non-attribute line is not `///` or `#[doc` - a proxy for rustc's "
           "missing_docs, which also counts fields and variants and was not run (cargo doc is a full build).",
    "compression": "zstd level 19 over each crate's product source concatenated: compressed / raw bytes. A "
                   "Kolmogorov-complexity PROXY - lower means more repetitive (boilerplate, tables, copy-paste); "
                   "it says nothing about correctness. Per file: the 10 lowest ratios among files >= 200 lines.",
    "graph": "Mermaid can't size nodes, so size is the label (product lines) and a size class; edge labels are "
             "use counts; colour = layer (unranked grey); a layer violation is drawn red.",
}


def _crate_of(path: str) -> str | None:
    parts = path.split("/")
    if parts[0] == "crates" and len(parts) > 2:
        return parts[1]
    if parts[0] == "tests":
        return "hk-e2e"
    return None


def cargo_edges(repo: str, sha: str) -> dict[str, dict[str, set]]:
    """{crate: {"normal": {deps}, "dev": {deps}}} from each crate's Cargo.toml blob at `sha`."""
    names = subprocess.run(["git", "-C", repo, "ls-tree", "-r", "--name-only", sha], capture_output=True, text=True,
                           check=True).stdout.split("\n")
    out = {}
    for path in names:
        if not re.match(r"^(crates/[^/]+|tests/e2e)/Cargo\.toml$", path):
            continue
        try:
            doc = tomllib.loads(subprocess.run(["git", "-C", repo, "show", f"{sha}:{path}"], capture_output=True,
                                               text=True, check=True).stdout)
        except Exception:
            continue
        name = (doc.get("package") or {}).get("name") or path.split("/")[1]
        sect = {"normal": set(doc.get("dependencies") or {}) | set(doc.get("build-dependencies") or {}),
                "dev": set(doc.get("dev-dependencies") or {})}
        for t in (doc.get("target") or {}).values():
            sect["normal"] |= set(t.get("dependencies") or {})
            sect["dev"] |= set(t.get("dev-dependencies") or {})
        out[name] = sect
    ws = set(out)
    return {c: {k: {d for d in v if d in ws and d != c} for k, v in s.items()} for c, s in out.items()}


def _product_lines(f: dict) -> list[str]:
    from hkpy.codemetrics import _test_cut
    cut = _test_cut(f)
    return [f["lines"][i] for i in f["code_idx"] if i < cut]


def gpl_closure(repo: str) -> dict:
    """GPL packages (no permissive alternative) in any workspace crate's normal-dep closure."""
    try:
        m = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked", "--offline"],
                                      cwd=repo, capture_output=True, text=True, timeout=120, check=True).stdout)
    except Exception as e:
        return {"error": f"{type(e).__name__}", "violations": None, "copyleft_with_alternative": []}
    pkgs = {p["id"]: p for p in m["packages"]}
    nodes = {n["id"]: n for n in (m.get("resolve") or {}).get("nodes", [])}
    members = set(m.get("workspace_members") or [])

    def lic(pid):
        return str(pkgs.get(pid, {}).get("license") or "")

    bad, alt = defaultdict(set), set()
    for root in members:
        seen, stack = set(), [root]
        while stack:
            pid = stack.pop()
            if pid in seen:
                continue
            seen.add(pid)
            for d in nodes.get(pid, {}).get("deps", []):
                if any(k.get("kind") in (None, "build") for k in d.get("dep_kinds", [])):
                    stack.append(d["pkg"])
        for pid in seen - {root}:
            text = lic(pid)
            if "GPL" in text:
                if any(p in text for p in _PERMISSIVE):
                    alt.add(f"{pkgs[pid]['name']} ({text})")
                else:
                    bad[pkgs[root]["name"]].add(f"{pkgs[pid]['name']} ({text})")
    return {"violations": sum(len(v) for v in bad.values()), "by_crate": {k: sorted(v) for k, v in bad.items()},
            "copyleft_with_alternative": sorted(alt), "packages": len(pkgs)}


def pub_api_churn(repo: str, sha: str, now: float, days: int = 7) -> dict[str, dict[str, int]]:
    from hkpy.codemetrics import is_test_path
    text = subprocess.run(["git", "-C", repo, "log", sha, "--no-merges", f"--since={int(now - days * 86400)}", "-p",
                           "--format=", "--", "crates/*.rs"], capture_output=True, text=True, timeout=180).stdout
    out: dict = defaultdict(Counter)
    crate = None
    for ln in text.splitlines():
        if ln.startswith("diff --git "):
            path = ln.split(" b/")[-1]
            crate = _crate_of(path) if not is_test_path(path, "Rust") else None
            continue
        if crate and ln[:1] in "+-" and not ln.startswith(("+++", "---")) and _PUB.match(ln[1:]):
            out[crate]["added" if ln[0] == "+" else "removed"] += 1
    return {k: dict(v) for k, v in out.items()}


def compressor():
    """(name, bytes -> compressed length): zstd -19 by the zstandard module, else the zstd CLI, else
    stdlib lzma (named, so the page never calls an lzma ratio zstd)."""
    try:
        import zstandard
        c = zstandard.ZstdCompressor(level=19)
        return "zstd -19", lambda b: len(c.compress(b))
    except ImportError:
        pass
    import shutil
    exe = shutil.which("zstd")
    if exe:
        return "zstd -19 (CLI)", lambda b: len(subprocess.run([exe, "-19", "-q", "-c"], input=b, capture_output=True,
                                                               check=True).stdout)
    import lzma
    return "lzma -9 (zstd unavailable)", lambda b: len(lzma.compress(b, preset=9))


def analyse(files: list[dict], repo: str, sha: str, now: float) -> dict:
    codec, clen = compressor()
    edges = cargo_edges(repo, sha)
    by_crate: dict = defaultdict(list)
    for f in files:
        c = _crate_of(f["path"])
        if c and f["lang"] == "Rust":
            by_crate[c].append(f)

    # --- per-crate product scans: idioms, pub items, missing docs, abstractness, compression
    per: dict = {}
    worst_files = []
    for c, fs in by_crate.items():
        idi, pubs, kinds, undoc, raw = Counter(), 0, Counter(), 0, []
        for f in fs:
            prod = _product_lines(f)
            text = "\n".join(prod)
            raw.append(text)
            for ln in prod:
                for k, rx in _IDIOM_RE.items():
                    if rx.search(ln):
                        idi[k] += 1
            # pub items and their docs, over the file's real line order
            from hkpy.codemetrics import _test_cut
            cut, lines = _test_cut(f), f["lines"]
            for i in range(min(cut, len(lines))):
                m = _PUB.match(lines[i])
                if not m:
                    continue
                pubs += 1
                kinds[m.group(1)] += 1
                if m.group(1) in ("use", "mod"):
                    continue
                j = i - 1
                while j >= 0 and lines[j].strip().startswith("#["):
                    j -= 1
                prev = lines[j].strip() if j >= 0 else ""
                if not (prev.startswith("///") or prev.startswith("#[doc") or prev.startswith("*/")):
                    undoc += 1
            if len(prod) >= 200:
                b = text.encode()
                worst_files.append({"path": f["path"], "lines": len(prod), "ratio": round(clen(b) / max(1, len(b)), 3)})
        blob = "\n".join(raw).encode()
        typed = sum(kinds[k] for k in ("struct", "enum", "trait", "union", "type"))
        per[c] = {"idioms": dict(idi), "pub_items": pubs, "pub_kinds": dict(kinds), "undocumented": undoc,
                  "abstractness": round(kinds["trait"] / typed, 3) if typed else 0.0,
                  "product_lines": sum(len(_product_lines(f)) for f in fs),
                  "zstd_ratio": round(clen(blob) / max(1, len(blob)), 3) if blob else None}
    worst_files.sort(key=lambda r: r["ratio"])

    # --- coupling and the layer rule
    ca = Counter(d for s in edges.values() for d in s["normal"])
    use_rx = {c: re.compile(r"\b" + re.escape(c.replace("-", "_")) + r"::") for c in edges}
    edge_rows, violations, dev_violations = [], [], []
    for c, s in edges.items():
        texts = ["\n".join(f["lines"]) for f in by_crate.get(c, [])]
        for d in sorted(s["normal"] | s["dev"]):
            uses = sum(len(use_rx[d].findall(t)) for t in texts)
            dev_only = d not in s["normal"]
            up = c in RANK and d in RANK and RANK[d] > RANK[c]
            row = {"from": c, "to": d, "uses": uses, "dev": dev_only, "violation": up and not dev_only}
            edge_rows.append(row)
            if up:
                (dev_violations if dev_only else violations).append(f"{c} -> {d}")
    coupling = []
    for c in sorted(edges):
        ce, a_ = len(edges[c]["normal"]), ca.get(c, 0)
        inst = round(ce / (ce + a_), 3) if ce + a_ else 0.0
        ab = per.get(c, {}).get("abstractness", 0.0)
        coupling.append({"name": c, "ca": a_, "ce": ce, "instability": inst, "abstractness": ab,
                         "distance": round(abs(ab + inst - 1), 3), "layer": RANK.get(c)})
    coupling.sort(key=lambda r: -r["distance"])

    # --- DSP-in-the-thin-client suspects
    ui_hits = []
    for f in files:
        if f["path"].startswith("ui/src/") and f["lang"] in ("TypeScript", "JS") and f["test"] == 0:
            for i in f["code_idx"]:
                m = _UI_DSP.search(f["lines"][i])
                if m:
                    ui_hits.append({"at": f"{f['path']}:{i + 1}", "word": m.group(1), "line": f["lines"][i].strip()[:140]})

    gpl = gpl_closure(repo)
    churn = pub_api_churn(repo, sha, now)
    return {
        "coupling": coupling, "edges": edge_rows,
        "rules": {"layer_violations": violations, "layer_violations_dev": dev_violations,
                  "gpl": gpl, "ui_dsp_suspects": ui_hits},
        "per_crate": [dict(name=c, **v, api_churn_7d=churn.get(c, {})) for c, v in
                      sorted(per.items(), key=lambda kv: -kv[1]["product_lines"])],
        "least_compressible": worst_files[-10:][::-1], "most_repetitive": worst_files[:10],
        "mermaid": mermaid(edge_rows, per), "codec": codec,
        "caveats": CAVEATS,
    }


def mermaid(edge_rows: list[dict], per: dict) -> str:
    """The crate graph: label = crate + product lines, class = layer (and size), red = violation."""
    lines = ["graph BT"]
    nodes = sorted({r["from"] for r in edge_rows} | {r["to"] for r in edge_rows} | set(per))
    for n in nodes:
        pl = per.get(n, {}).get("product_lines", 0)
        size = "s3" if pl >= 15000 else "s2" if pl >= 5000 else "s1"
        cls = f"L{RANK[n]}" if n in RANK else "unranked"
        lines.append(f'  {n.replace("-", "_")}["{n}<br/>{pl:,}"]:::{cls}')
        lines.append(f"  class {n.replace('-', '_')} {size}")
    link = 0
    red = []
    for r in edge_rows:
        if r["dev"]:
            continue
        lines.append(f'  {r["from"].replace("-", "_")} -->|{r["uses"]}| {r["to"].replace("-", "_")}')
        if r["violation"]:
            red.append(link)
        link += 1
    palette = ["#1f3b4d", "#24485c", "#2a566b", "#30637a", "#367089", "#3c7e98", "#428ba7", "#4898b6", "#4ea6c5", "#54b3d4"]
    for i, col in enumerate(palette):
        lines.append(f"  classDef L{i} fill:{col},stroke:#9fd0c0,color:#fff")
    lines += ["  classDef unranked fill:#2b2f33,stroke:#5A6973,color:#D5DEE2",
              "  classDef s1 font-size:11px", "  classDef s2 font-size:14px", "  classDef s3 font-size:18px"]
    if red:
        lines.append(f"  linkStyle {','.join(map(str, red))} stroke:#E47B68,stroke-width:3px")
    return "\n".join(lines)
