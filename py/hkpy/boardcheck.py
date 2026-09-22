"""The board's validator, at the point where the board is WRITTEN.

WHY THIS EXISTS, AND WHY THE GATE WAS NOT ENOUGH. On 2026-09-22 T-640's title contained
``sources[iq-ring].available: true``, unquoted. To a YAML loader that colon-space opens a nested
mapping, so ``yaml.safe_load`` rejected the WHOLE file and the dashboard's task graph went blank.
Every textual guard in ``py/tests/test_task_board.py`` passed, ``hkpy.reconcile`` parses leniently
and was fine, and the gate stayed green.

T-762 closed the gate half: the board test now strict-parses with the same loader the dashboard
uses. But the break did not arrive through a gated merge. It arrived through a DIRECT BOARD
COMMIT — "Board: flip nine tickets", "File T-700" — and those run no gate at all, several times an
hour. A gate-layer check would not have caught it and will not catch the next one; the board's
integrity rested on whoever was committing remembering to look.

So this module is the check, and ``.githooks/pre-commit`` is where it runs: before ANY commit that
touches ``docs/tasks.yaml``, whoever makes it — a coordinator flipping statuses by hand, or
``ops/merge-runner.sh`` committing a staged merge. ``hkpy.boardmerge._validate`` calls the same
function, so the merge driver vouches for exactly what the hook vouches for.

IT FAILS CLOSED, deliberately. If the board cannot be read, if ``pyyaml`` is missing, if the
checker cannot run at all — the answer is REFUSE, never "assume fine". A validator that silently
no-ops is how this class of defect recurs, and it is the same shape as a nextest override naming a
package no run could see (T-631) and a staged-bulk marker written where its only reader never
looked (T-761). For the same reason success is announced with a token line (``board-ok: …``) that
the hook must SEE: a checker replaced by ``true`` would exit 0 and prove nothing.

TAKING THE DEPENDENCY, EXPLICITLY. ``pyyaml`` was long refused for ``py/`` and ``hkpy.boardmerge``
still says so. T-762 changed that decision on the evidence — the one failure that has ever mattered
here is invisible to a textual scan, because the loader is the thing that rejects the file — and
declared ``pyyaml`` (MIT) a dev dependency, locked in ``py/uv.lock``. Both writers already run
Python through ``uv run --locked --project py`` (the merge driver has since T-582), so the loader
is present on both paths without a new install step, and absent it we refuse. The stdlib checks
below still run first, so the *specific* construct that broke the board is named in plain language
rather than as a parser error.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ID_RE = re.compile(r"^  - id: (T-\d+[a-z]?)\s*$")
KEY_RE = re.compile(r"^    ([a-z_]+): ?(.*)$")
OK_TOKEN = "board-ok"


def _scalar_colon_problems(text: str) -> list[str]:
    """Plain scalars containing ``": "`` — the only construct that has ever broken this file.

    Reported before the parse so the message says *which field of which ticket* and what to do
    about it, instead of "mapping values are not allowed here" at line 9214.
    """
    out: list[str] = []
    tid = "?"
    for line in text.splitlines():
        m = ID_RE.match(line)
        if m:
            tid = m.group(1)
            continue
        k = KEY_RE.match(line)
        if not k:
            continue
        key, val = k.group(1), k.group(2).strip()
        if val[:1] in ("", "'", '"', "|", ">", "[", "{", "#", "&", "*"):
            continue
        if ": " in val:
            out.append(
                f"{tid}: {key}: contains ': ' unquoted, so YAML reads it as a nested mapping "
                f"and rejects the file — quote it: {val[:70]}"
            )
    return out


def problems(text: str, *, min_tasks: int = 100) -> list[str]:
    """Every reason this board text must not be committed. Empty list == it may.

    `min_tasks` is the floor a healthy board must clear: a parse that succeeds but yields a
    handful of tickets is a damaged file, not a small one. The merge driver passes the count its
    own textual split found, so "the loader saw fewer tickets than the text has" is a refusal.
    """
    out = _scalar_colon_problems(text)
    try:
        import yaml  # noqa: PLC0415 - optional at import time so the message can be ours
    except ImportError:  # pragma: no cover - environment-dependent
        return out + [
            "pyyaml is not importable, so the board cannot be parsed with the loader its "
            "consumers use; run this under `uv run --locked --project py` (py/uv.lock pins it). "
            "Refusing rather than assuming the board is fine."
        ]
    try:
        doc = yaml.safe_load(text)
    except yaml.YAMLError as e:
        return out + [f"docs/tasks.yaml is not strict YAML; the dashboard cannot load it:\n{e}"]
    if not isinstance(doc, dict):
        return out + [f"the board is not a mapping at the top level (got {type(doc).__name__})"]
    tasks = doc.get("tasks")
    if not isinstance(tasks, list) or len(tasks) < min_tasks:
        n = len(tasks) if isinstance(tasks, list) else "no"
        return out + [
            f"strict parse found {n} tasks, fewer than the {min_tasks} expected; that is a "
            "damaged board, not a small one"
        ]
    ids = [str(t.get("id")) for t in tasks if isinstance(t, dict)]
    if len(ids) != len(tasks):
        out.append("some task entries are not mappings")
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes:
        out.append(f"duplicate ticket ids: {dupes} — renumber one side (never reuse an id)")
    return out


def check_text(text: str) -> tuple[int, str]:
    """-> (exit code, message). The success message carries the token the hook looks for."""
    bad = problems(text)
    if bad:
        return 1, "board-check: REFUSING this board:\n  " + "\n  ".join(bad)
    n = text.count("\n  - id: ")
    return 0, f"{OK_TOKEN}: {n} tickets, strict-parses"


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: boardcheck <docs/tasks.yaml | ->", file=sys.stderr)
        return 2
    src = argv[1]
    try:
        text = sys.stdin.read() if src == "-" else Path(src).read_text(encoding="utf-8")
    except OSError as e:
        # Unreadable is REFUSED, not skipped: "we could not look" must never read as "it is fine".
        print(f"board-check: REFUSING — cannot read the board ({e})", file=sys.stderr)
        return 1
    code, msg = check_text(text)
    print(msg, file=sys.stderr if code else sys.stdout)
    return code


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
