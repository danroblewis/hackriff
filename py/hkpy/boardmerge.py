"""A git merge driver for `docs/tasks.yaml`.

WHY THIS EXISTS. The board is one YAML file whose task list has a single append point, just
before the top-level `notes:` key. Any two branches that file a ticket append at the same line
and conflict — every time, and always resolving identically to "keep both", carrying no
disagreement whatever. On 2026-09-21 that happened NINE times in one night; one of them cost a
fourteen-branch batch its shared gate. A conflict that always resolves the same way is a
conflict the file format is manufacturing, so the resolution belongs in a runner rather than in
whoever is awake (the same argument as T-396 for the gate and T-477 for reconcile).

WHAT IT DOES, AND WHAT IT REFUSES. It automates exactly one case: both sides only APPENDED
tickets, and nothing else moved. Then the union is unambiguous and it writes it. In every other
case — the same ticket edited on both sides, a ticket deleted, any change outside the task list —
it exits non-zero and git falls back to an ordinary conflict for a person.

That narrowness is the point. A plain `union` merge driver on YAML would happily produce a file
that PARSES and is WRONG (two `status:` lines in one block, a half-merged acceptance string). A
malformed board already reached main once, on 2026-09-20, because `docs/` classified as prose and
the docs class ran nothing. So this driver validates before it writes: the result must parse, must
contain every ticket from both sides, and must have no duplicate ids. If any of that fails it
refuses rather than guessing.

Text in, text out: blocks are moved verbatim rather than re-serialised, so an untouched ticket's
formatting, comment style and string folding survive a merge byte for byte.
"""

from __future__ import annotations

import re
import sys

# Ids are not all `T-<digits>`: the real board carries `T-022a`, a sub-ticket suffix. Missing
# that made this driver absorb one ticket into its neighbour, which the test against the real
# file caught — a regex fitted to the ids I expected rather than the ids that exist.
ID_RE = re.compile(r"^  - id: (T-\d+[a-z]?)\s*$")
TRAILER_RE = re.compile(r"^[a-zA-Z_][\w-]*:")  # a top-level key after the task list


def split(text: str) -> tuple[str, dict[str, str], list[str], str]:
    """-> (header, {id: block}, id order, trailer). Blocks are verbatim text."""
    lines = text.split("\n")
    first = None
    for i, line in enumerate(lines):
        if ID_RE.match(line):
            first = i
            break
    if first is None:
        return text, {}, [], ""
    end = len(lines)
    for i in range(first, len(lines)):
        if TRAILER_RE.match(lines[i]):
            end = i
            break
    header = "\n".join(lines[:first])
    trailer = "\n".join(lines[end:])
    blocks: dict[str, str] = {}
    order: list[str] = []
    cur_id: str | None = None
    cur: list[str] = []
    for line in lines[first:end]:
        m = ID_RE.match(line)
        if m:
            if cur_id is not None:
                blocks[cur_id] = "\n".join(cur)
            cur_id, cur = m.group(1), [line]
            order.append(cur_id)
        else:
            cur.append(line)
    if cur_id is not None:
        blocks[cur_id] = "\n".join(cur)
    return header, blocks, order, trailer


def merge(base: str, ours: str, theirs: str) -> str | None:
    """The append-only union, or None when a person must decide."""
    bh, bb, bo, bt = split(base)
    oh, ob, oo, ot = split(ours)
    th, tb, to, tt = split(theirs)
    if not bo and not oo and not to:
        return None
    # Anything outside the task list is not ours to reconcile.
    if oh != th or ot != tt:
        return None
    # A ticket touched on both sides, or deleted on either, is a real disagreement.
    for tid in bb:
        if tid not in ob or tid not in tb:
            return None
        o_ch, t_ch = ob[tid] != bb[tid], tb[tid] != bb[tid]
        if o_ch and t_ch and ob[tid] != tb[tid]:
            return None
    # Both sides filing the SAME new id is an id collision, not an append. The union would
    # silently keep one branch's ticket and drop the other's, which is worse than a conflict:
    # it loses work without telling anyone. (It happened for real on 2026-09-21, when two
    # branches both took T-571..T-573.)
    for tid in set(ob) & set(tb) - set(bb):
        if ob[tid] != tb[tid]:
            return None
    merged: dict[str, str] = {}
    for tid, blk in bb.items():
        merged[tid] = tb[tid] if tb[tid] != blk else ob[tid]
    order = list(bo)
    for side_order, side_blocks in ((oo, ob), (to, tb)):
        for tid in side_order:
            if tid not in merged:
                merged[tid] = side_blocks[tid]
                order.append(tid)
    return oh + "\n" + "\n".join(merged[t] for t in order) + "\n" + ot


def _validate(text: str, need: set[str]) -> str | None:
    """Stdlib only, like `py/tests/test_task_board.py` — `pyyaml` is not a dependency here.

    Blocks are moved verbatim, so the risks worth checking are structural: a lost ticket, a
    duplicated id, a block that bled into its neighbour, or a mangled tail. A YAML parser would
    add little over this and a dependency the project has deliberately refused.
    """
    _, blocks, order, trailer = split(text)
    if not order:
        return "result has no task list"
    dupes = sorted({t for t in order if order.count(t) > 1})
    if dupes:
        return f"duplicate ids: {dupes}"
    missing = sorted(need - set(order))
    if missing:
        return f"tickets lost in the merge: {missing}"
    for tid, blk in blocks.items():
        for line in blk.split("\n")[1:]:
            if line and not line.startswith("    "):
                return f"{tid}: a line escaped its block: {line[:40]!r}"
    if trailer and not TRAILER_RE.match(trailer.split("\n")[0]):
        return "the tail after the task list is not a top-level key"
    return None


def main(argv: list[str]) -> int:
    if len(argv) < 4:
        print("usage: boardmerge <base> <ours> <theirs>", file=sys.stderr)
        return 2
    base_p, ours_p, theirs_p = argv[1], argv[2], argv[3]
    base, ours, theirs = (open(p, encoding="utf-8").read() for p in (base_p, ours_p, theirs_p))
    out = merge(base, ours, theirs)
    if out is None:
        print("board-merge: not an append-only merge; leaving it to a person", file=sys.stderr)
        return 1
    need = set(split(ours)[1]) | set(split(theirs)[1])
    why = _validate(out, need)
    if why:
        print(f"board-merge: refusing — {why}", file=sys.stderr)
        return 1
    with open(ours_p, "w", encoding="utf-8") as fh:
        fh.write(out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
