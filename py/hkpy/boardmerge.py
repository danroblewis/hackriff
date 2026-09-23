"""A git merge driver for `docs/tasks.yaml`.

WHY THIS EXISTS. The board is one YAML file whose task list has a single append point, just
before the top-level `notes:` key. Any two branches that file a ticket append at the same line
and conflict — every time, and always resolving identically to "keep both", carrying no
disagreement whatever. On 2026-09-21 that happened NINE times in one night; one of them cost a
fourteen-branch batch its shared gate. A conflict that always resolves the same way is a
conflict the file format is manufacturing, so the resolution belongs in a runner rather than in
whoever is awake (the same argument as T-396 for the gate and T-477 for reconcile).

WHAT IT DOES, AND WHAT IT REFUSES. It automates two cases. (1) Both sides only APPENDED
tickets, and nothing else moved: the union is unambiguous and it writes it. (2) The same ticket
changed on both sides but in DIFFERENT FIELDS - since 2026-09-23, when the work runner's own
two writes (`status: in-progress` on main at dispatch, `result:` on the branch at hand-back)
made every worker landing a conflict a person resolved identically seven times in a day: each
field is taken from the side that changed it (`merge_block`). In every other case — the same
field changed two ways, a ticket deleted, any change outside the task list — it exits non-zero
and git falls back to an ordinary conflict for a person.

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


FIELD_RE = re.compile(r"^    ([a-z_]+):")  # a ticket's own top-level key, 4-space indented


def fields(block: str) -> tuple[str, dict[str, str], list[str]] | None:
    """-> (the `- id:` line, {key: verbatim field text with its continuation lines}, key order).

    None when the block is not a plain list of fields (a line before the first key, or a key
    twice), which is exactly the shape `_validate` refuses too - so the caller refuses as well.
    """
    lines = block.split("\n")
    out: dict[str, str] = {}
    order: list[str] = []
    cur: str | None = None
    for line in lines[1:]:
        m = FIELD_RE.match(line)
        if m:
            cur = m.group(1)
            if cur in out:
                return None
            order.append(cur)
            out[cur] = line
        elif cur is not None:
            out[cur] += "\n" + line
        elif line.strip():
            return None
    return lines[0], out, order


def merge_block(base: str, ours: str, theirs: str) -> str | None:
    """One ticket changed on BOTH sides: merge field by field, or None when a field disagrees.

    2026-09-23: the work runner flips a dispatched ticket to `status: in-progress` (+ `branch:`)
    on main, then writes its `result:` on the worker's branch at hand-back - so EVERY worker
    branch changed the same block as main, the block-level rule above refused it, and seven
    landings that day each waited for a person to type the resolution the two sides already
    agreed on. Disjoint keys carry no disagreement; the same key changed two ways still does.
    Fields move verbatim (a `result: |` and its continuation lines are one field), and the
    result passes `_validate` like any other resolution.
    """
    b, o, t = fields(base), fields(ours), fields(theirs)
    if b is None or o is None or t is None or not (b[0] == o[0] == t[0]):
        return None
    bf, of, tf = b[1], o[1], t[1]
    merged: dict[str, str | None] = {}
    for key in set(bf) | set(of) | set(tf):
        bv, ov, tv = bf.get(key), of.get(key), tf.get(key)
        if ov == bv:
            merged[key] = tv       # ours untouched: theirs decides (None = theirs removed it)
        elif tv == bv or ov == tv:
            merged[key] = ov
        else:
            return None            # the same key changed two ways: a real disagreement
    order = [k for k in b[2] if merged.get(k) is not None]
    for side in (o[2], t[2]):
        for k in side:
            if k not in order and merged.get(k) is not None:
                order.append(k)
    if not order:
        return None
    return "\n".join([b[0]] + [merged[k] for k in order])  # type: ignore[misc]


def merge(base: str, ours: str, theirs: str) -> str | None:
    """The append-only union (plus field-disjoint edits of one block), or None when a person must decide."""
    bh, bb, bo, bt = split(base)
    oh, ob, oo, ot = split(ours)
    th, tb, to, tt = split(theirs)
    if not bo and not oo and not to:
        return None
    # Anything outside the task list is not ours to reconcile.
    if oh != th or ot != tt:
        return None
    # A ticket deleted on either side is a real disagreement. One touched on both sides is
    # merged field by field, and is a disagreement only where the same field changed two ways.
    resolved: dict[str, str] = {}
    for tid in bb:
        if tid not in ob or tid not in tb:
            return None
        o_ch, t_ch = ob[tid] != bb[tid], tb[tid] != bb[tid]
        if o_ch and t_ch and ob[tid] != tb[tid]:
            m = merge_block(bb[tid], ob[tid], tb[tid])
            if m is None:
                return None
            resolved[tid] = m
    # Both sides filing the SAME new id is an id collision, not an append. The union would
    # silently keep one branch's ticket and drop the other's, which is worse than a conflict:
    # it loses work without telling anyone. (It happened for real on 2026-09-21, when two
    # branches both took T-571..T-573.)
    for tid in set(ob) & set(tb) - set(bb):
        if ob[tid] != tb[tid]:
            return None
    merged: dict[str, str] = {}
    for tid, blk in bb.items():
        merged[tid] = resolved.get(tid) or (tb[tid] if tb[tid] != blk else ob[tid])
    order = list(bo)
    for side_order, side_blocks in ((oo, ob), (to, tb)):
        for tid in side_order:
            if tid not in merged:
                merged[tid] = side_blocks[tid]
                order.append(tid)
    return oh + "\n" + "\n".join(merged[t] for t in order) + "\n" + ot


def _validate(text: str, need: set[str]) -> str | None:
    """What the driver vouches for before it writes a resolution with NO HUMAN IN THE LOOP.

    Blocks are moved verbatim, so the structural risks are first: a lost ticket, a duplicated id,
    a block that bled into its neighbour, a mangled tail.

    It then vouches for the property those checks cannot see, via `hkpy.boardcheck`: that the
    result LOADS under the strict parser its consumers use. This file used to argue that a YAML
    parser "would add little over this and a dependency the project has deliberately refused" —
    that was wrong, and 2026-09-22 is why: a board no loader accepts passed every textual guard
    in the tree and blanked the dashboard. T-762 took the dependency deliberately (`pyyaml`, MIT,
    a dev dependency of `py/`, locked), and this driver has run under `uv run --locked --project
    py` since T-582, so it is present here. `boardcheck` refuses when it is not, which is the
    answer that fails closed.
    """
    from hkpy import boardcheck  # noqa: PLC0415 - keeps the merge path's imports where they are used

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
        # A key twice in one block is what a LOST `- id:` line looks like: the swallowed ticket's
        # keys land inside its neighbour. YAML will not object — a duplicate key silently keeps the
        # last value — so the swallowed body wins and the host's is dead in the parse. This
        # validator missed it once, on 2026-09-21, and passed a resolution that reintroduced damage
        # the board test then caught; checking here is what makes the driver's refusal trustworthy.
        seen: set[str] = set()
        for key in re.findall(r"^    ([a-z_]+):", blk, re.M):
            if key in seen:
                return f"{tid}: declares {key!r} twice, which is a lost `- id:` line"
            seen.add(key)
    if trailer and not TRAILER_RE.match(trailer.split("\n")[0]):
        return "the tail after the task list is not a top-level key"
    bad = boardcheck.problems(text, min_tasks=len(order))
    if bad:
        return "; ".join(bad)
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
