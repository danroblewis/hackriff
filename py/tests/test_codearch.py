"""/metrics phase 2 part A (py/hkpy/codearch.py; user, 2026-09-24)."""
from __future__ import annotations

import json
import subprocess
import time

from hkpy import codearch as A
from hkpy import codemetrics as C

LIB = """/// documented
pub fn a() -> u32 { x().unwrap() }
pub struct S;
pub trait T {}
#[derive(Debug)]
pub enum E { A }
static N: u32 = 1;
fn f(v: Arc<Mutex<u32>>, d: Box<dyn Fn()>) { unsafe { g() } }
#[cfg(test)]
mod tests { fn t() { y().unwrap(); } }
"""


def _repo(tmp_path):
    r = tmp_path / "r"
    files = {
        "crates/hk-model/Cargo.toml": '[package]\nname = "hk-model"\n',
        "crates/hk-model/src/lib.rs": LIB,
        "crates/hk-core/Cargo.toml": '[package]\nname = "hk-core"\n[dependencies]\nhk-model.workspace = true\nhk-dsp = { path = "../hk-dsp" }\nserde = "1"\n[dev-dependencies]\nhk-api.workspace = true\n',
        "crates/hk-core/src/lib.rs": "use hk_model::S;\nfn z() { hk_model::a(); hk_dsp::q(); }\n",
        "crates/hk-dsp/Cargo.toml": '[package]\nname = "hk-dsp"\n[dependencies]\nhk-model.workspace = true\n',
        "crates/hk-dsp/src/lib.rs": "pub fn q() {}\n",
        "crates/hk-api/Cargo.toml": '[package]\nname = "hk-api"\n[dependencies]\nhk-core.workspace = true\n',
        "crates/hk-api/src/lib.rs": "pub fn r() {}\n",
        "ui/src/view/a.ts": "const n = fftSize;\nexport const b = 1;\n",
    }
    for p, body in files.items():
        f = r / p
        f.parent.mkdir(parents=True, exist_ok=True)
        f.write_text(body)
    g = lambda *a: subprocess.run(["git", "-C", str(r), "-c", "user.email=t@t", "-c", "user.name=t", *a], check=True, capture_output=True)  # noqa: E731
    g("init", "-q", "-b", "main")
    g("add", "-A")
    g("commit", "-qm", "one")
    return r


def test_edges_coupling_and_the_layer_rule(tmp_path, monkeypatch):
    r = _repo(tmp_path)
    monkeypatch.setattr(A, "gpl_closure", lambda repo: {"violations": 0, "copyleft_with_alternative": []})
    files = C.read_tree(str(r), "main")
    a = A.analyse(files, str(r), "main", time.time())
    assert A.cargo_edges(str(r), "main")["hk-core"] == {"normal": {"hk-model", "hk-dsp"}, "dev": {"hk-api"}}   # serde is not ours
    assert a["rules"]["layer_violations"] == ["hk-core -> hk-dsp"]          # core sits BELOW dsp
    assert a["rules"]["layer_violations_dev"] == ["hk-core -> hk-api"]
    e = {(x["from"], x["to"]): x for x in a["edges"]}
    assert e[("hk-core", "hk-model")]["uses"] == 2 and e[("hk-core", "hk-dsp")]["violation"]
    cp = {x["name"]: x for x in a["coupling"]}
    assert (cp["hk-model"]["ca"], cp["hk-model"]["ce"], cp["hk-model"]["instability"]) == (2, 0, 0.0)
    assert cp["hk-core"]["ce"] == 2 and cp["hk-core"]["ca"] == 1
    assert "linkStyle" in a["mermaid"] and "stroke:#E47B68" in a["mermaid"]


def test_idioms_api_docs_and_ui_suspects_count_product_code_only(tmp_path, monkeypatch):
    r = _repo(tmp_path)
    monkeypatch.setattr(A, "gpl_closure", lambda repo: {"violations": 0, "copyleft_with_alternative": []})
    a = A.analyse(C.read_tree(str(r), "main"), str(r), "main", time.time())
    m = {p["name"]: p for p in a["per_crate"]}["hk-model"]
    assert m["idioms"]["unwrap()/expect()"] == 1                             # the test module's unwrap is not counted
    assert m["idioms"]["Arc<Mutex|RwLock>"] == 1 and m["idioms"]["dyn Trait"] == 1 and m["idioms"]["unsafe"] == 1
    assert m["idioms"]["static/OnceCell/lazy"] == 1
    assert m["pub_items"] == 4 and m["undocumented"] == 3                    # a() is documented; the derive is skipped
    assert m["abstractness"] == round(1 / 3, 3)                              # 1 trait of struct+trait+enum
    assert [h["word"] for h in a["rules"]["ui_dsp_suspects"]] == ["fftSize"]


def test_gpl_needs_no_permissive_alternative(tmp_path, monkeypatch):
    meta = {"workspace_members": ["ws"], "packages": [
        {"id": "ws", "name": "hk-x", "license": None},
        {"id": "g", "name": "gplonly", "license": "GPL-3.0"},
        {"id": "d", "name": "dual", "license": "MIT OR Apache-2.0 OR LGPL-2.1-or-later"},
        {"id": "t", "name": "devonly", "license": "GPL-3.0"}],
        "resolve": {"nodes": [{"id": "ws", "deps": [{"pkg": "g", "dep_kinds": [{"kind": None}]},
                                                     {"pkg": "d", "dep_kinds": [{"kind": None}]},
                                                     {"pkg": "t", "dep_kinds": [{"kind": "dev"}]}]}]}}

    class P:
        stdout = json.dumps(meta)
    monkeypatch.setattr(A.subprocess, "run", lambda *a, **k: P())
    g = A.gpl_closure(".")
    assert g["violations"] == 1 and g["by_crate"] == {"hk-x": ["gplonly (GPL-3.0)"]}
    assert g["copyleft_with_alternative"] == ["dual (MIT OR Apache-2.0 OR LGPL-2.1-or-later)"]
