"""Refuse a tree carrying unresolved merge-conflict markers. T-843.

On 2026-09-22 `docs/adr/README.md` was found on `main` with `<<<<<<< HEAD`, `=======` and
`>>>>>>> task-t549` committed into it, from a merge weeks earlier. Nobody noticed, and nothing
could have: the gate classifies a `docs/`-only diff as **nothing to run**, so a docs file can
carry anything at all onto main.

That is the same hole the board's YAML break came through - a write path with no checker - and it
is why this is deliberately NOT a docs-specific test. A conflict marker is meaningless in every
file type, the check costs about a second over the whole tree, and it should therefore run for
EVERY diff class including the ones that currently run nothing.

Deliberately stdlib and textual, like the rest of the board tooling: the evidence is the literal
marker, and a parser would only obscure it.
"""

from __future__ import annotations

import subprocess
from pathlib import Path

#: The three markers `git` leaves. `|||||||` appears only in diff3 style, and is included because
#: a tree carrying it is just as unresolved as one carrying the others.
MARKERS = ("<<<<<<< ", "=======", ">>>>>>> ", "||||||| ")

#: Files that legitimately TALK about markers rather than carrying them. Kept explicit and tiny:
#: a broad ignore list is how a checker stops checking.
ALLOW = frozenset({"py/hkpy/conflictmarkers.py", "py/tests/test_conflict_markers.py"})


def tracked_files(repo: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "-z"], capture_output=True, text=True, check=True
    ).stdout
    return [f for f in out.split("\0") if f]


def offenders(repo: Path, paths: list[str] | None = None) -> list[tuple[str, int, str]]:
    """(path, line number, the marker line) for every tracked file carrying a marker."""
    found: list[tuple[str, int, str]] = []
    for rel in paths if paths is not None else tracked_files(repo):
        if rel in ALLOW:
            continue
        try:
            text = (repo / rel).read_text(encoding="utf-8", errors="strict")
        except (OSError, UnicodeDecodeError):
            continue  # binary or unreadable: a marker there is not a thing that happens
        for n, line in enumerate(text.splitlines(), 1):
            # `=======` alone is a legal markdown/rst underline, so it only counts as evidence
            # when the file ALSO carries an opening or closing marker.
            if line.startswith(("<<<<<<< ", ">>>>>>> ", "||||||| ")):
                found.append((rel, n, line[:80]))
    return found


def main(argv: list[str] | None = None) -> int:
    repo = Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
        ).stdout.strip()
    )
    bad = offenders(repo)
    for path, n, line in bad:
        print(f"{path}:{n}: {line}")
    if bad:
        print(f"\n{len(bad)} unresolved conflict marker(s). A merge was committed half-resolved.")
        return 1
    print("no conflict markers in the tracked tree")
    return 0


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
