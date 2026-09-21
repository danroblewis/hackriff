"""Which Rust crates a diff can possibly affect (T-543).

`py/hkpy/gate.py` answers *which suites* a diff needs. This answers the next question down:
given that the diff is in `crates/`, which **packages' test binaries could its change reach**
— so a one-crate edit does not have to run and re-link all twenty.

THE RULE, and it is a linkage rule, not a judgement call. A package's test binaries are
rebuilt and re-run when the change is anywhere in what they LINK:

  * a package's **lib** links its normal and build dependencies, transitively;
  * a package's **test targets** link its lib *plus* its dev-dependencies, and each
    dev-dependency's own lib closure with it.

So the affected set of a changed package C is::

    L        = C, plus every package whose lib transitively depends on C
    affected = L, plus every package with a dev-dependency in L

Both halves are load-bearing and the second is the one a naive implementation drops.
`crates/hk-dsp/Cargo.toml` has ``[dev-dependencies] hk-e2e = { path = "../../tests/e2e" }``
for its synthetic scenarios, and `hk-e2e`'s own lib depends on `hk-pipeline`, `hk-cli`,
`hk-api` and most of the rest. hk-dsp's *test binary* therefore links hk-cli's code, and
dropping dev edges would silently stop running tests that do link the change.

WHAT THIS BUYS HERE, MEASURED on this workspace's 20 packages (2026-09-20). The number that
decides it is that `hk-e2e`'s **lib** depends only on `hk-model`; everything else it names —
`hk-pipeline`, `hk-cli`, `hk-api`, `hk-plugins`, `hk-sim` and the rest — is a
*dev*-dependency of `hk-e2e`, so it reaches hk-e2e's own test targets and stops there rather
than flowing on into the eight crates that dev-depend on hk-e2e::

    crates/hk-cli/**        ->  2   hk-cli hk-e2e
    crates/hk-sim/**        ->  2
    crates/hk-pipeline/**   ->  3   hk-cli hk-e2e hk-pipeline
    crates/hk-api/**        ->  4
    crates/hk-plugins/**    ->  4
    crates/hk-detect/**     ->  5
    crates/hk-ml/**         ->  6
    crates/hk-recipe/**     -> 12
    crates/hk-dsp/**        -> 14
    crates/hk-stream/**     -> 15
    crates/hk-model/**      -> 20  (the whole workspace: everything depends on the model)

So the common single-crate edit runs 2-5 packages of 20, and the deep foundational ones
honestly run nearly all of them. If the dev-dependency edges were dropped the answers would
be smaller and WRONG; if the distinction between hk-e2e's lib and its dev-deps were dropped
they would all be ~16 and the lever would be worthless. Both halves have to be right.

WHAT IT DELIBERATELY DOES NOT NARROW. `hk-e2e` is in the affected set of **every** crate in
the workspace (it dev-depends on fourteen of them and lib-depends on `hk-model`), so the
acceptance suite runs for every `crates/` change, always. That is the suite that catches the
class of defect this ticket is forbidden to trade away — a green unit suite shipping a dark
demo (T-484) — and the graph happens to agree with the policy rather than having to be
overridden by it.

FAIL CLOSED, the same discipline as `gate.py`. `select()` returns ``None`` — meaning "run
the whole workspace" — whenever the answer is not certain:

  * `cargo metadata` is unavailable or unparseable;
  * a changed path under `crates/` or `tests/` maps to no package;
  * the diff touches anything that can change how the *whole* workspace builds or what the
    suites see: the workspace `Cargo.toml`/`Cargo.lock`, `.config/` (the nextest thread cap
    and serial groups), `fixtures/`, `plugins/`, `recipes/`, the `justfile`, `.github/`, or
    any path `gate.py` could not classify.

The last bullet is why this module takes the *whole* changed-path list rather than only the
paths that happen to start with `crates/`: a selection made while ignoring `Cargo.lock` is a
selection made on the wrong graph.

Stdlib only, like `gate.py`, so `python3 py/hkpy/crates.py` works as well as `just gate`.
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass, field

#: Paths that force the whole workspace even though they sit next to, or inside, crates.
#: Each one can change what every package builds or what every suite sees, so no
#: per-package answer derived from them is trustworthy.
_WORKSPACE_WIDE_EXACT = ("Cargo.toml", "Cargo.lock", "justfile", "rust-toolchain.toml")
_WORKSPACE_WIDE_PREFIX = (".config/", ".github/", "fixtures/", "plugins/", "recipes/")


@dataclass(frozen=True)
class Workspace:
    """The workspace dependency graph, as `cargo metadata` reports it."""

    #: package name -> its manifest directory, repo-relative with forward slashes.
    dirs: dict[str, str] = field(default_factory=dict)
    #: package name -> normal + build dependencies inside the workspace.
    lib_deps: dict[str, set[str]] = field(default_factory=dict)
    #: package name -> dev-dependencies inside the workspace.
    dev_deps: dict[str, set[str]] = field(default_factory=dict)

    @property
    def names(self) -> set[str]:
        return set(self.dirs)


def load_workspace(root: str) -> Workspace | None:
    """Read the workspace graph. `None` if cargo cannot answer — the fail-closed case.

    `--no-deps` keeps this to the workspace's own packages and makes it fast (tens of
    milliseconds), and it is read from the **post-change** tree, so a diff that edits a
    crate's `Cargo.toml` is classified against the graph it creates rather than the one it
    replaces.
    """
    try:
        proc = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            cwd=root,
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0 or not proc.stdout.strip():
            return None
        meta = json.loads(proc.stdout)
    except Exception:
        # Any failure at all — no cargo, a broken manifest, unparseable output, a stubbed
        # subprocess in a test — means the graph is unknown, and unknown means the whole
        # workspace. The broad catch IS the fail-closed behaviour, not a swallowed bug.
        return None
    return parse_metadata(meta, root)


def parse_metadata(meta: dict, root: str) -> Workspace | None:
    """Turn `cargo metadata --no-deps` output into a `Workspace`. Pure, so it is testable."""
    packages = meta.get("packages")
    if not isinstance(packages, list) or not packages:
        return None
    root_abs = os.path.abspath(root)
    dirs: dict[str, str] = {}
    lib_deps: dict[str, set[str]] = {}
    dev_deps: dict[str, set[str]] = {}
    names = set()
    for pkg in packages:
        name = pkg.get("name")
        if isinstance(name, str):
            names.add(name)
    for pkg in packages:
        name = pkg.get("name")
        manifest = pkg.get("manifest_path")
        if not isinstance(name, str) or not isinstance(manifest, str):
            return None
        pkg_dir = os.path.dirname(os.path.abspath(manifest))
        try:
            rel = os.path.relpath(pkg_dir, root_abs).replace(os.sep, "/")
        except ValueError:
            return None
        if rel.startswith(".."):
            # A package outside the repo: no path in the diff can map to it, and its
            # presence means the graph is not the one this diff describes.
            return None
        dirs[name] = "" if rel == "." else rel
        lib: set[str] = set()
        dev: set[str] = set()
        for dep in pkg.get("dependencies", []):
            dname = dep.get("name")
            if dname not in names:
                continue
            # `kind` is null for a normal dependency, "dev" or "build" otherwise.
            if dep.get("kind") == "dev":
                dev.add(dname)
            else:
                lib.add(dname)
        lib_deps[name] = lib
        dev_deps[name] = dev
    return Workspace(dirs=dirs, lib_deps=lib_deps, dev_deps=dev_deps)


def _norm(path: str) -> str:
    """Repo-relative, forward slashes, no leading `./` — and NOT `lstrip("./")`.

    `lstrip` strips a *character set*, so `".config/nextest.toml".lstrip("./")` is
    `"config/nextest.toml"` and the `.config/` rule below never fires. A dot-directory that
    silently stops matching the rule that forces the full workspace is exactly the
    fail-OPEN this module must not have, so the normalisation is one named function with a
    test, the same shape as `gate.normalize`.
    """
    p = path.strip().replace("\\", "/")
    while p.startswith("./"):
        p = p[2:]
    return p.strip("/") if p.endswith("/") else p


def package_of(ws: Workspace, path: str) -> str | None:
    """The package a changed path belongs to, by longest matching manifest directory.

    Longest match, not first match: a workspace whose root manifest sits at `""` would
    otherwise swallow every path.
    """
    p = _norm(path)
    best: tuple[int, str] | None = None
    for name, d in ws.dirs.items():
        if not d:
            continue
        if p == d or p.startswith(d + "/"):
            if best is None or len(d) > best[0]:
                best = (len(d), name)
    return None if best is None else best[1]


def affected(ws: Workspace, changed: set[str]) -> set[str]:
    """Every package whose test binaries could link a change in `changed`.

    Two passes, exactly as the module docstring states: the reverse-lib closure, then the
    packages that reach it through a dev-dependency.
    """
    rev_lib: dict[str, set[str]] = {}
    for pkg, deps in ws.lib_deps.items():
        for dep in deps:
            rev_lib.setdefault(dep, set()).add(pkg)

    lib_closure = set(changed)
    stack = list(changed)
    while stack:
        cur = stack.pop()
        for rdep in rev_lib.get(cur, ()):
            if rdep not in lib_closure:
                lib_closure.add(rdep)
                stack.append(rdep)

    out = set(lib_closure)
    for pkg, deps in ws.dev_deps.items():
        if deps & lib_closure:
            out.add(pkg)
    return out


def forces_workspace(path: str) -> str | None:
    """Reason this path forbids any per-crate narrowing, or `None`.

    These are the paths whose effect is not expressible in the dependency graph at all: the
    lock file and workspace manifest change what everything resolves to, `.config/` changes
    how the runner schedules every test, `fixtures/`, `plugins/` and `recipes/` change what
    the suites read at run time, and the `justfile`/`.github/` are the gate itself.
    """
    p = _norm(path)
    if p in _WORKSPACE_WIDE_EXACT:
        return f"{p} changes how the whole workspace builds"
    for prefix in _WORKSPACE_WIDE_PREFIX:
        if p.startswith(prefix):
            return f"{prefix} is read by the suites at run time, not through the dep graph"
    return None


@dataclass(frozen=True)
class Selection:
    """The answer, with the reasoning that produced it.

    `crates is None` means **run the whole workspace** — the fail-closed answer, and the
    only answer that is never wrong.
    """

    crates: tuple[str, ...] | None
    reason: str
    #: The packages the diff actually edited, before the closure was taken.
    changed: tuple[str, ...] = ()

    @property
    def is_workspace(self) -> bool:
        return self.crates is None


def select(paths, ws: Workspace | None) -> Selection:
    """Pick the crate set for a set of changed paths, or the whole workspace.

    Only paths that live inside a package narrow anything. Paths outside `crates/` and
    `tests/` that are *not* in the workspace-wide list — `ui/`, `docs/`, `py/` — belong to
    classes that do not run the Rust suite at all, so they neither narrow nor widen: the
    caller has already decided the class, and this function only refines the `full` one.
    """
    if ws is None:
        return Selection(None, "cargo metadata unavailable — running the whole workspace")

    changed_pkgs: set[str] = set()
    for raw in paths:
        p = _norm(str(raw))
        if not p:
            continue
        why = forces_workspace(p)
        if why is not None:
            return Selection(None, why)
        pkg = package_of(ws, p)
        if pkg is not None:
            changed_pkgs.add(pkg)
            continue
        # Inside the Rust tree but belonging to no package: a new crate nobody has added to
        # the workspace yet, or a stray file. Either way the graph does not describe it.
        if p.startswith("crates/") or p.startswith("tests/"):
            return Selection(None, f"{p} is under the Rust tree but in no workspace package")

    if not changed_pkgs:
        return Selection(None, "no workspace package changed — nothing to narrow to")

    chosen = affected(ws, changed_pkgs)
    if chosen >= ws.names:
        return Selection(
            None,
            "the affected closure is the whole workspace — no narrowing available",
            changed=tuple(sorted(changed_pkgs)),
        )
    return Selection(
        tuple(sorted(chosen)),
        f"{len(chosen)} of {len(ws.names)} packages can link a change in "
        + ", ".join(sorted(changed_pkgs)),
        changed=tuple(sorted(changed_pkgs)),
    )


def main(argv: list[str] | None = None) -> int:  # pragma: no cover - thin CLI
    import argparse

    parser = argparse.ArgumentParser(
        prog="python3 py/hkpy/crates.py",
        description="Print the crate selection for a list of changed paths.",
    )
    parser.add_argument("paths", nargs="*", help="changed paths (repo-relative)")
    parser.add_argument("--root", default=None)
    args = parser.parse_args(argv)
    root = args.root or os.getcwd()
    sel = select(args.paths, load_workspace(root))
    if sel.is_workspace:
        print(f"crates: WHOLE WORKSPACE — {sel.reason}")
    else:
        print(f"crates: {' '.join(sel.crates or ())}")
        print(f"crates: reason — {sel.reason}")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
