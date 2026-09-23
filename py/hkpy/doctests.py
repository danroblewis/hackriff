"""Which workspace crates can possibly contain a doctest.

`just test-doc` used to run ``cargo test --workspace --exclude hk-e2e --doc``: **19 crates
invoked through rustdoc to execute 2 doctests** (`hk_blocks` 1, `hk_recipe` 1). Every other
crate prints ``0 passed; 0 failed`` — and each of those zeroes still costs a full
``rustdoc --test`` of the crate, which is not fingerprinted and so is paid on every single
gate. That was measured as a large share of `just test`'s unattributed ~270 s
(``docs/test-speed-review-2026-09-22.md`` §1.2 / R7).

**The rule, derived and never maintained by hand.** A doctest can only come from a code fence
inside a doc comment or a ``#[doc = …]`` attribute. So a crate whose ``src/`` contains no such
fence cannot contain a doctest, and running rustdoc over it can only ever print zero. This
module finds the fences; it does not hold a list of crates somebody has to remember to update.
That distinction is the whole point: R7's stated objection to narrowing ``--doc`` was that a
hand-kept list "silently stops testing the next doctest written". A derivation cannot — the
moment someone writes ``/// ```" in a crate, that crate is back in the set, with no edit here.

**It fails closed.** Anything this module is unsure of — a crate it cannot read, a doc
attribute whose content comes from somewhere else (``#[doc = include_str!(…)]``), a scan that
raises — puts the crate **in** the set. The expensive answer is the safe one, exactly as
`hkpy.crates` and `hkpy.gate` do one level up.

Used by ``just _doctest-scope``; ``just test-doc`` prints what it selected and what it skipped,
so a narrowed run always says so.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]

#: Directories holding workspace members, matching `Cargo.toml`'s `members`.
MEMBER_GLOBS = ("crates/*", "tests/e2e")


def crate_dirs(repo: Path) -> list[Path]:
    """Every workspace member directory that has a `Cargo.toml`, sorted by package name."""
    out: list[Path] = []
    for pat in MEMBER_GLOBS:
        for d in sorted(repo.glob(pat)):
            if (d / "Cargo.toml").is_file():
                out.append(d)
    return out


def package_name(crate: Path) -> str | None:
    """The `name = "…"` from `[package]`, or None if it cannot be read."""
    try:
        text = (crate / "Cargo.toml").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    in_package = False
    for raw in text.splitlines():
        line = raw.strip()
        if line.startswith("["):
            in_package = line == "[package]"
            continue
        if in_package and line.startswith("name"):
            _, _, val = line.partition("=")
            return val.strip().strip('"')
    return None


#: Fence info-string words rustdoc itself understands. A fence whose info string is empty or
#: made only of these is a Rust doctest — `ignore` included, because rustdoc still *counts* it.
#: Anything else (`text`, `json`, `bash`, …) marks the block as not-Rust and rustdoc skips it.
RUSTDOC_ATTRS = {
    "rust", "ignore", "should_panic", "no_run", "compile_fail", "test_harness",
    "standalone_crate", "edition2015", "edition2018", "edition2021", "edition2024",
}


def has_doc_fence(text: str) -> bool:
    """Whether `text` holds a code fence that rustdoc would turn into a doctest.

    A doctest comes from a ``` fence inside a `///` / `//!` doc comment, or from a
    `#[doc = …]` attribute. An `include_str!`/`concat!` doc attribute counts unconditionally:
    its content is not in this file, so the honest answer is "might".

    The info string decides. 48 of this workspace's 58 doc fences are ```` ```text ```` and
    another 4 are ```` ```json ````/```` ```jsonc ````; rustdoc does not run any of those. So
    the scan is a **state machine** over each run of doc-comment lines: an opening fence records
    its info string, the next fence closes the block (and a closing fence's empty info string is
    not a second opening one, which a line-by-line grep gets wrong every time).
    """
    open_info: str | None = None
    for raw in text.splitlines():
        line = raw.lstrip()
        if line.startswith("///") or line.startswith("//!"):
            body = line[3:].lstrip()
            if not body.startswith("```"):
                continue
            if open_info is not None:
                open_info = None  # this fence closes the open block
                continue
            info = body.lstrip("`").strip()
            words = {w.strip() for w in info.replace(",", " ").split() if w.strip()}
            if not words or words <= RUSTDOC_ATTRS:
                return True
            open_info = info
        elif line.startswith("#[doc") or line.startswith("#![doc"):
            if "```" in line or "include_str!" in line or "concat!" in line:
                return True
        else:
            # Out of the doc block; an unterminated fence does not leak into the next item.
            open_info = None
    return False


def crate_has_doctests(crate: Path) -> bool:
    """Whether `crate`'s `src/` can contain a doctest. True on any read failure (fail closed)."""
    src = crate / "src"
    if not src.is_dir():
        # No lib target to run doctests against — but say yes rather than guess about a layout
        # this rule was not written for.
        return True
    try:
        for f in src.rglob("*.rs"):
            if has_doc_fence(f.read_text(encoding="utf-8", errors="replace")):
                return True
    except OSError:
        return True
    return False


def scope(repo: Path, exclude: str = "", gate_crates: str | None = None) -> tuple[list[str], list[str]]:
    """`(selected, skipped)` package names: those that may hold a doctest, and those that cannot.

    `exclude` drops one package outright (hk-e2e, which `just test` has never run).
    `gate_crates` is `$HK_GATE_CRATES` — when set, the selection is intersected with it, the same
    opt-in narrowing `hkpy.crates` feeds `_crate-scope`.
    """
    wanted = set(gate_crates.split()) if gate_crates else None
    selected: list[str] = []
    skipped: list[str] = []
    for d in crate_dirs(repo):
        name = package_name(d)
        if name is None or name == exclude:
            continue
        if wanted is not None and name not in wanted:
            continue
        (selected if crate_has_doctests(d) else skipped).append(name)
    return sorted(selected), sorted(skipped)


def main(argv: list[str]) -> int:
    exclude = argv[1] if len(argv) > 1 else ""
    try:
        selected, skipped = scope(REPO, exclude, os.environ.get("HK_GATE_CRATES"))
    except Exception as exc:  # noqa: BLE001 — fail closed: the whole workspace, loudly.
        print(f"doctest scope: scan failed ({exc}); running the WHOLE workspace", file=sys.stderr)
        print(f"--workspace --exclude {exclude}" if exclude else "--workspace")
        return 0
    if not selected:
        # Nothing to run. Say so on stdout as an empty scope; the recipe turns that into a skip
        # rather than into `cargo test --doc` with no package (which would mean "current package").
        print("doctest scope: no crate in scope holds a doc-comment code fence", file=sys.stderr)
        print("")
        return 0
    print(
        f"doctest scope: {len(selected)} crate(s) with doc fences ({' '.join(selected)}); "
        f"{len(skipped)} without, not invoked through rustdoc ({' '.join(skipped)})",
        file=sys.stderr,
    )
    print(" ".join(f"-p {p}" for p in selected))
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main(sys.argv))
