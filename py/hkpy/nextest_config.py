"""T-631: a nextest override must be reachable by a nextest run that reads it.

`.config/nextest.toml` is only ever consulted by `cargo nextest`. An override whose
`filter` names a package is therefore **inert** unless some nextest invocation in the
gate can actually select tests from that package — and an inert override is worse than a
missing one, because everyone reasons about a protection that is not applied.

That is exactly what happened. The file's FIRST override pinned `package(hk-e2e)` into the
serial `heavy-serial` group and its header explained why hk-e2e is heavy; meanwhile every
recipe that runs hk-e2e used plain `cargo test` (which never reads this file) and every
recipe that used nextest passed `just _crate-scope hk-e2e`, i.e. `--workspace --exclude
hk-e2e`. 125 tests believed themselves serialised for months of commits and were not.

So this module compares the two files that have to agree:

* the **packages named** in `.config/nextest.toml`'s override filters, and
* the **package scope of the gate's nextest invocations**, read out of the `justfile`.

and fails when a named package is outside every one of them, or names no workspace member
at all (the same defect by typo). Parameterised recipes — `just test-crate <crate>`, `just
test-one <name>` — are deliberately NOT counted: their scope comes from whoever types them,
so they can never be evidence that the config is exercised by anything. Had they counted,
this check would have passed on the very tree that produced T-631.

Pure text; no build, no cargo. Run it with `just nextest-config-check`.
"""

from __future__ import annotations

import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

#: A `package(name)` token inside a nextest filterset expression.
_PACKAGE = re.compile(r"\bpackage\(\s*([A-Za-z0-9_.\-]+)\s*\)")
#: `filter = '...'` / `filter = "..."` in `.config/nextest.toml`.
_FILTER = re.compile(r"^\s*filter\s*=\s*(['\"])(.*)\1\s*$")
#: A `cargo nextest run ...` command line in the justfile.
_NEXTEST_RUN = re.compile(r"\bcargo\s+nextest\s+run\b(.*)$")
#: `scope=$(just _crate-scope hk-e2e)` — the one indirection the justfile uses.
_SCOPE_ASSIGN = re.compile(r"scope=\$\(\s*just\s+_crate-scope\s*([A-Za-z0-9_\-]*)\s*\)")
#: A just recipe header: `name arg="":` at column 0.
_RECIPE = re.compile(r"^([a-zA-Z0-9_-]+)[^:=\n]*:")


@dataclass(frozen=True)
class Scope:
    """The packages one `cargo nextest run` can select tests from."""

    recipe: str
    line: str
    #: True when the invocation runs the whole workspace minus `excluded`.
    workspace: bool = False
    included: frozenset[str] = frozenset()
    excluded: frozenset[str] = frozenset()
    #: The scope is a recipe parameter, so it proves nothing about what the gate runs.
    parameterised: bool = False

    def sees(self, package: str, members: frozenset[str]) -> bool:
        if self.parameterised:
            return False
        if self.workspace:
            return package in members and package not in self.excluded
        return package in self.included


@dataclass
class Report:
    members: frozenset[str] = frozenset()
    scopes: list[Scope] = field(default_factory=list)
    #: package -> the filter lines that named it.
    named: dict[str, list[str]] = field(default_factory=dict)
    problems: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not self.problems


def workspace_members(repo: Path = REPO) -> frozenset[str]:
    """Package names of the Cargo workspace members, read from their manifests."""
    root = (repo / "Cargo.toml").read_text(encoding="utf-8")
    body = root.split("[workspace]", 1)[1]
    raw = re.search(r"members\s*=\s*\[(.*?)\]", body, re.S)
    if raw is None:
        raise ValueError("Cargo.toml has no [workspace] members list")
    names: set[str] = set()
    for pattern in re.findall(r"['\"]([^'\"]+)['\"]", raw.group(1)):
        for manifest in sorted(repo.glob(f"{pattern}/Cargo.toml")):
            name = re.search(
                r"^\s*name\s*=\s*['\"]([^'\"]+)['\"]",
                manifest.read_text(encoding="utf-8"),
                re.M,
            )
            if name:
                names.add(name.group(1))
    return frozenset(names)


def overrides(config: Path) -> dict[str, list[str]]:
    """Package name -> the `filter = ...` lines of `.config/nextest.toml` that name it."""
    named: dict[str, list[str]] = {}
    for raw in config.read_text(encoding="utf-8").splitlines():
        m = _FILTER.match(raw)
        if not m:
            continue
        for package in _PACKAGE.findall(m.group(2)):
            named.setdefault(package, []).append(raw.strip())
    return named


def nextest_scopes(justfile: Path) -> list[Scope]:
    """The package scope of every `cargo nextest run` in the justfile."""
    scopes: list[Scope] = []
    recipe = "<file>"
    pending_exclude: str | None = None
    for raw in justfile.read_text(encoding="utf-8").splitlines():
        if not raw.startswith((" ", "\t", "#")) and (m := _RECIPE.match(raw)):
            recipe, pending_exclude = m.group(1), None
        if a := _SCOPE_ASSIGN.search(raw):
            pending_exclude = a.group(1) or None
        stripped = raw.strip()
        if stripped.startswith("#"):
            continue  # a comment that merely talks about the command
        run = _NEXTEST_RUN.search(stripped)
        if not run:
            continue
        args = run.group(1)
        # Only a PACKAGE SELECTOR that is a recipe parameter makes the scope unknowable.
        # `{{args}}` passed through to the runner does not (T-631's own `_e2e-run` ends
        # `-p hk-e2e -E "$expr" {{args}}`, and its package is perfectly well known).
        if re.search(r"(?:-p|--package)\s+\{\{", args):
            scopes.append(Scope(recipe, stripped, parameterised=True))
        elif "$scope" in args or "$(just _crate-scope" in args:
            excl = pending_exclude
            if inline := re.search(r"\$\(\s*just\s+_crate-scope\s*([A-Za-z0-9_\-]*)\s*\)", args):
                excl = inline.group(1) or None
            scopes.append(
                Scope(
                    recipe,
                    stripped,
                    workspace=True,
                    excluded=frozenset({excl} if excl else ()),
                )
            )
        elif "--workspace" in args:
            scopes.append(Scope(recipe, stripped, workspace=True))
        elif packages := re.findall(r"-p\s+([A-Za-z0-9_\-]+)", args):
            scopes.append(Scope(recipe, stripped, included=frozenset(packages)))
        else:
            # No package selector at all: cargo defaults to the current package, which in a
            # workspace root is nothing. Fail closed — an unrecognised form proves nothing.
            scopes.append(Scope(recipe, stripped, parameterised=True))
    return scopes


def check(repo: Path = REPO) -> Report:
    """Every package named by a nextest override must be visible to a gate nextest run."""
    report = Report(
        members=workspace_members(repo),
        scopes=nextest_scopes(repo / "justfile"),
        named=overrides(repo / ".config" / "nextest.toml"),
    )
    real = [s for s in report.scopes if not s.parameterised]
    if not real:
        report.problems.append(
            "the justfile has no unparameterised `cargo nextest run`, so no override in "
            ".config/nextest.toml can apply to anything"
        )
    for package, filters in sorted(report.named.items()):
        where = "; ".join(filters)
        if package not in report.members:
            report.problems.append(
                f"`package({package})` in .config/nextest.toml names no workspace member "
                f"(it can never match a test): {where}"
            )
            continue
        if not any(s.sees(package, report.members) for s in real):
            excluders = ", ".join(
                f"`{s.recipe}` ({s.line})" for s in real if package in s.excluded
            )
            report.problems.append(
                f"`package({package})` in .config/nextest.toml is INERT: every nextest run "
                f"in the justfile excludes it, so the override has never applied to a test. "
                f"Either run that package under nextest or delete the override (and move its "
                f"reasoning somewhere that is true). Excluded by: {excluders or 'no run selects it'}"
                f" — filter: {where}"
            )
    return report


def main(argv: list[str] | None = None) -> int:
    report = check()
    if report.ok:
        print(
            f"nextest-config-check: {len(report.named)} package(s) named by overrides, all "
            f"visible to {sum(1 for s in report.scopes if not s.parameterised)} gate nextest "
            f"run(s)"
        )
        return 0
    for problem in report.problems:
        print(f"nextest-config-check: {problem}", file=sys.stderr)
    return 1


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
