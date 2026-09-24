"""The task-board CLI: the only sanctioned way to edit `docs/tasks.yaml`.

WHY THIS EXISTS. The board is a single 20k+ line YAML file, hand-edited by every agent and the
coordinator, and it kept breaking: unquoted `: ` inside a title turned into a nested mapping and
blanked the dashboard (T-640), an all-digit `commit:` value silently became an octal int, two
sessions allocated the same `T-nnn` id on different branches on the same day. None of those needed
a person to be careless — the file format punishes a raw text edit for mistakes a person cannot see
at the point of typing. User directive, 2026-09-22: "they should actually be banned from editing
tasks.yaml directly." A `PreToolUse` hook (`.claude/hooks/block-board-edits.sh`) enforces that in
worktrees; this module is what the hook tells agents to use instead.

HOW IT EDITS. Every write is TEXT-LEVEL, on the one ticket's block, exactly like
`hkpy.boardmerge`'s append-only merge driver and `hkpy.reconcile`'s textual scan: it never
round-trips the whole document through a YAML dumper, because that would reformat all 20k+ lines
and turn every future diff into noise. `boardmerge.split()` is reused verbatim to find a block's
extent — `(header, {id: block_text}, id_order, trailer)` — and edits happen inside one block's
text before the file is reassembled from the same pieces.

EVERY WRITE IS CHECKED BEFORE IT IS TRUSTED. After writing, the module re-reads the file, and
`yaml.safe_load` must succeed with ids still unique; if not, the ORIGINAL bytes are restored and
the command exits non-zero with the parser's message. A command that cannot prove its own edit is
safe leaves the file exactly as it found it.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

import yaml

from hkpy.boardmerge import split

# The board's own status vocabulary (py/tests/test_task_board.py,
# test_status_is_from_the_boards_own_vocabulary): a status this CLI or `reconcile` does not
# recognise is a ticket invisible to both. Adding a state is a decision taken there, not here.
STATUS_VOCAB = {"todo", "in-progress", "blocked", "done", "deferred", "cancelled", "reverted", "planned"}  # planned: in the plan, not scheduled (2026-09-22)

# Characters/patterns that make a YAML plain scalar ambiguous or illegal unquoted. Mirrors the
# faults `test_task_board.py` actually checks for (an all-digit value reading as an int/octal, a
# ": " turning a scalar into a nested mapping) plus the usual plain-scalar indicators.
_NEEDS_QUOTE = re.compile(r"^[!&*\-?:,\[\]{}#|>'\"%@`]|: |:$")
_NUMERIC = re.compile(r"^[+-]?\d+(\.\d+)?$")
_RESERVED = {"true", "false", "null", "~", "yes", "no", "on", "off"}


def repo_root() -> Path:
    out = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    )
    return Path(out.stdout.strip())


def default_board() -> Path:
    return repo_root() / "docs" / "tasks.yaml"


def scalar(value: str) -> str:
    """A YAML plain scalar for `value`, quoted only when a bare scalar would be misread.

    Auto-quoting means a caller never has to know the board's YAML gotchas (an all-digit commit
    SHA silently becoming an octal int is the one that has actually bitten this file) — `set`
    always produces a value that reads back as the same string.
    """
    if value == "" or value.strip() != value:
        quoted = True
    elif _NEEDS_QUOTE.search(value) or _NUMERIC.match(value) or value.lower() in _RESERVED:
        quoted = True
    else:
        quoted = False
    if not quoted:
        return value
    escaped = value.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def die(msg: str) -> "int":
    print(f"task: {msg}", file=sys.stderr)
    return 1


# --------------------------------------------------------------------------------------------
# Reading
# --------------------------------------------------------------------------------------------


def load_doc(text: str) -> dict:
    """Strict parse — the same loader `ops/monitor.py` and the board test use."""
    doc = yaml.safe_load(text)
    if not isinstance(doc, dict):
        raise yaml.YAMLError("top level of tasks.yaml is not a mapping")
    return doc


def find_block(text: str, tid: str) -> tuple[str, dict[str, str], list[str], str]:
    """`(header, blocks, order, trailer)`, asserting `tid` is present (raises KeyError if not)."""
    header, blocks, order, trailer = split(text)
    if tid not in blocks:
        raise KeyError(tid)
    return header, blocks, order, trailer


def reassemble(header: str, blocks: dict[str, str], order: list[str], trailer: str) -> str:
    return header + "\n" + "\n".join(blocks[t] for t in order) + "\n" + trailer


# --------------------------------------------------------------------------------------------
# Writing, checked
# --------------------------------------------------------------------------------------------


def write_checked(path: Path, original_text: str, new_text: str) -> dict:
    """Write, re-read, and prove the result is still a valid board — or restore and fail.

    Restoring on failure means a caller can retry or fix its input without the file ever having
    carried a broken intermediate state, even for the instant between write and check.
    """
    path.write_text(new_text, encoding="utf-8")
    reread = path.read_text(encoding="utf-8")
    try:
        doc = load_doc(reread)
    except yaml.YAMLError as e:
        path.write_text(original_text, encoding="utf-8")
        raise SystemExit(die(f"write produced invalid YAML, restored the original file:\n{e}"))
    tasks = doc.get("tasks") or []
    ids = [str(t.get("id")) for t in tasks]
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes:
        path.write_text(original_text, encoding="utf-8")
        raise SystemExit(die(f"write produced duplicate ids {dupes}, restored the original file"))
    return doc


# --------------------------------------------------------------------------------------------
# show
# --------------------------------------------------------------------------------------------


def cmd_show(args: argparse.Namespace) -> int:
    text = args.file.read_text(encoding="utf-8")
    try:
        _, blocks, _, _ = find_block(text, args.id)
    except KeyError:
        return die(f"no such ticket: {args.id}")
    print(blocks[args.id])
    return 0


# --------------------------------------------------------------------------------------------
# list
# --------------------------------------------------------------------------------------------


def _ready(t: dict, by_id: dict[str, dict]) -> bool:
    if t.get("status") != "todo":
        return False
    if t.get("blocked_on"):
        return False
    if t.get("needs") in ("user", "hardware"):
        return False
    for dep in t.get("depends_on") or t.get("deps") or []:  # the runner reads both spellings
        dep_t = by_id.get(str(dep))
        if dep_t is None or dep_t.get("status") not in ("done", "cancelled"):
            return False
    return True


def cmd_list(args: argparse.Namespace) -> int:
    text = args.file.read_text(encoding="utf-8")
    try:
        doc = load_doc(text)
    except yaml.YAMLError as e:
        return die(f"docs/tasks.yaml is not strict YAML:\n{e}")
    tasks = doc.get("tasks") or []
    by_id = {str(t.get("id")): t for t in tasks}

    rows = []
    for t in tasks:
        if args.status and t.get("status") != args.status:
            continue
        if args.milestone and str(t.get("milestone", "")) != args.milestone:
            continue
        if args.group and str(t.get("parallel_group", "")) != args.group:
            continue
        if args.ready and not _ready(t, by_id):
            continue
        rows.append(t)

    if args.json:
        import json

        print(
            json.dumps(
                [
                    {
                        "id": t.get("id"),
                        "status": t.get("status"),
                        "priority": t.get("priority"),
                        "model": t.get("model"),
                        "parallel_group": t.get("parallel_group"),
                        "milestone": t.get("milestone"),
                        "title": t.get("title"),
                    }
                    for t in rows
                ],
                indent=2,
            )
        )
        return 0

    for t in rows:
        title = (t.get("title") or "").replace("\n", " ")[:70]
        print(
            f"{t.get('id', '?'):<10} {t.get('status', '-'):<12} {t.get('priority') or '-':<8} "
            f"{t.get('model') or '-':<8} {t.get('parallel_group') or '-':<8} "
            f"{t.get('milestone') or '-':<10} {title}"
        )
    return 0


# --------------------------------------------------------------------------------------------
# set
# --------------------------------------------------------------------------------------------

_BLOCK_MARKERS = {"", "|", "|-", "|+", ">", ">-", ">+"}


def _set_scalar_field(block: str, key: str, value: str) -> str:
    """Replace `key`'s single-line value in `block`, or append the field if absent.

    Refuses (raises `ValueError`) when `key`'s value spans more than one physical line — a
    literal/folded block scalar (`|`/`>`) or a quoted scalar YAML itself wrapped across lines
    (T-330's title does this: a plain single-quoted string with a continuation line indented six
    spaces). Overwriting just the header line in either case would strand the indented
    continuation as orphaned text the next parse cannot place — exactly the corruption shape
    `boardmerge`'s validator watches for.
    """
    lines = block.split("\n")
    pattern = re.compile(rf"^    {re.escape(key)}: (.*)$")
    for i, line in enumerate(lines):
        m = pattern.match(line)
        if not m:
            continue
        existing = m.group(1).strip()
        continues = i + 1 < len(lines) and lines[i + 1].startswith("      ")
        if existing in _BLOCK_MARKERS or continues:
            raise ValueError(
                f"{key!r} spans more than one line (a block scalar or a wrapped quoted string); "
                "use `just task note`/`result` for block fields, or edit it by hand and re-run "
                "`just task validate`"
            )
        lines[i] = f"    {key}: {_field_value(value)}"
        return "\n".join(lines)
    lines.append(f"    {key}: {_field_value(value)}")
    return "\n".join(lines)


def _field_value(value: str) -> str:
    """`value` as YAML: a `[a, b]` flow list (each item quoted only when it must be) or a scalar.

    A list is written only for the bracketed form, so `depends_on=[T-1, T-2]` reads back as a real
    list (the runner's `deps_of` calls `list()` on it — a quoted string would become its characters)
    while every other value keeps `scalar`'s auto-quoting. `[]` writes an empty list.
    """
    v = value.strip()
    if v in ("true", "false"):
        return v  # a real boolean, as the board writes core_interface / is_hil (never the string)
    if v.startswith("[") and v.endswith("]"):
        items = [x.strip() for x in v[1:-1].split(",") if x.strip()]
        return "[" + ", ".join(scalar(x) for x in items) + "]"
    return scalar(value)


def _unset_field(block: str, key: str) -> tuple[str, bool]:
    """Remove `key` and every continuation line under it (a `|`/`>` block or a wrapped scalar)."""
    lines = block.split("\n")
    pattern = re.compile(rf"^    {re.escape(key)}:(\s|$)")
    for i, line in enumerate(lines):
        if pattern.match(line):
            # The field ends at the next line indented at key level (4 spaces then text); a blank
            # line inside a `|` block belongs to the field only if the block continues after it.
            j = i + 1
            while j < len(lines):
                if lines[j].startswith("      "):
                    j += 1
                elif lines[j].strip() == "" and any(
                    ln.startswith("      ") for ln in lines[j + 1:j + 2]
                ):
                    j += 1
                else:
                    break
            return "\n".join(lines[:i] + lines[j:]), True
    return block, False


def cmd_unset(args: argparse.Namespace) -> int:
    original = args.file.read_text(encoding="utf-8")
    try:
        header, blocks, order, trailer = find_block(original, args.id)
    except KeyError:
        return die(f"no such ticket: {args.id}")
    block = blocks[args.id]
    for key in args.keys:
        if key in ("id", "title", "status"):
            return die(f"refusing to unset {key!r} (every ticket carries it)")
        block, found = _unset_field(block, key)
        if not found:
            return die(f"{args.id} has no {key!r}")
    status_m = re.search(r"^    status: (.*)$", block, re.M)
    if status_m and status_m.group(1).strip() == "blocked" and not re.search(r"^    blocked_on:", block, re.M):
        return die("refusing to remove blocked_on from a blocked ticket (set its status first)")
    blocks[args.id] = block
    write_checked(args.file, original, reassemble(header, blocks, order, trailer))
    print(f"task: updated {args.id}")
    return 0


def cmd_set(args: argparse.Namespace) -> int:
    original = args.file.read_text(encoding="utf-8")
    try:
        header, blocks, order, trailer = find_block(original, args.id)
    except KeyError:
        return die(f"no such ticket: {args.id}")

    block = blocks[args.id]
    kvs: list[tuple[str, str]] = []
    for kv in args.assignments:
        key, sep, val = kv.partition("=")
        if not sep or not key:
            return die(f"expected key=value, got {kv!r}")
        kvs.append((key.strip(), val))

    for key, val in kvs:
        if key == "status" and val not in STATUS_VOCAB:
            return die(f"unknown status {val!r}; the board's vocabulary is {sorted(STATUS_VOCAB)}")
        try:
            block = _set_scalar_field(block, key, val)
        except ValueError as e:
            return die(str(e))

    # A ticket may not be `blocked` without a `blocked_on` (user, 2026-09-16;
    # py/tests/test_task_board.py). Checked on the FINAL block so `set T-1 status=blocked
    # blocked_on=...` in one call is allowed.
    status_m = re.search(r"^    status: (.*)$", block, re.M)
    blocked_on_m = re.search(r"^    blocked_on:\s*(.*)$", block, re.M)
    if status_m and status_m.group(1).strip() == "blocked":
        if not blocked_on_m or not blocked_on_m.group(1).strip():
            return die("refusing status=blocked with no blocked_on set (what, specifically, unblocks it?)")

    blocks[args.id] = block
    new_text = reassemble(header, blocks, order, trailer)
    write_checked(args.file, original, new_text)
    print(f"task: updated {args.id}")
    return 0


# --------------------------------------------------------------------------------------------
# result / note — block-scalar fields
# --------------------------------------------------------------------------------------------


def _find_block_scalar(lines: list[str], key: str) -> tuple[int, int] | None:
    """`(header_index, end_index)` for `key`'s `|`/`>` block in `lines`, else None.

    `end_index` is exclusive: `lines[header_index + 1 : end_index]` is the body, already indented
    six spaces. The body ends at the first line that is non-blank and indented four spaces or
    less — the next top-level field, or the end of the ticket block.
    """
    pat = re.compile(rf"^    {re.escape(key)}:\s*[|>][-+0-9]*\s*$")
    for i, line in enumerate(lines):
        if pat.match(line):
            j = i + 1
            while j < len(lines):
                ln = lines[j]
                if ln.strip() == "" or ln.startswith("      "):
                    j += 1
                    continue
                break
            return i, j
    return None


def _prefixed(body: str) -> list[str]:
    return ["      " + ln if ln else "" for ln in body.splitlines()]


def _set_block_scalar(block: str, key: str, body: str, *, append: bool) -> str:
    lines = block.split("\n")
    found = _find_block_scalar(lines, key)
    new_body_lines = _prefixed(body)
    if found:
        i, j = found
        if append:
            existing_body = lines[i + 1 : j]
            new_body_lines = existing_body + [""] + new_body_lines
        lines[i + 1 : j] = new_body_lines
    else:
        lines.extend([f"    {key}: |"] + new_body_lines)
    return "\n".join(lines)


def _body_text(args: argparse.Namespace) -> str:
    if args.file_arg:
        return Path(args.file_arg).read_text(encoding="utf-8").rstrip("\n")
    return args.text.rstrip("\n") if args.text else ""


def _cmd_block_scalar(args: argparse.Namespace, key: str, *, append: bool) -> int:
    original = args.file.read_text(encoding="utf-8")
    try:
        header, blocks, order, trailer = find_block(original, args.id)
    except KeyError:
        return die(f"no such ticket: {args.id}")

    body = _body_text(args)
    if not body:
        return die("nothing to write: pass --file or --text")

    blocks[args.id] = _set_block_scalar(blocks[args.id], key, body, append=append)
    new_text = reassemble(header, blocks, order, trailer)
    write_checked(args.file, original, new_text)
    print(f"task: {'appended to' if append else 'set'} {key} on {args.id}")
    return 0


def cmd_result(args: argparse.Namespace) -> int:
    return _cmd_block_scalar(args, "result", append=False)


def cmd_note(args: argparse.Namespace) -> int:
    return _cmd_block_scalar(args, "notes", append=True)


# --------------------------------------------------------------------------------------------
# new
# --------------------------------------------------------------------------------------------


def _max_id_num(text: str) -> int:
    nums = [int(m.group(1)) for m in re.finditer(r"^  - id: T-(\d+)", text, re.M)]
    return max(nums, default=0)


def next_id(working_text: str) -> str:
    """`T-{n+1}` for the highest `n` seen in the working file OR the tip of any local branch.

    Two sessions on different branches both computing `max(working file) + 1` collide the moment
    both branches merge (2026-09-22: T-761..T-764 filed twice). Checking every branch tip narrows
    but does not close the window between two agents both running `new` before either merges —
    that is a real race, and `validate`'s duplicate-id check is the backstop for it, not this.
    """
    best = _max_id_num(working_text)
    out = subprocess.run(
        ["git", "for-each-ref", "--format=%(refname:short)", "refs/heads"],
        capture_output=True,
        text=True,
        check=False,
    )
    for ref in out.stdout.split():
        show = subprocess.run(
            ["git", "show", f"{ref}:docs/tasks.yaml"], capture_output=True, text=True, check=False
        )
        if show.returncode == 0:
            best = max(best, _max_id_num(show.stdout))
    return f"T-{best + 1}"


def _build_new_block(tid: str, args: argparse.Namespace) -> str:
    lines = [f"  - id: {tid}"]
    if args.milestone:
        lines.append(f"    milestone: {scalar(args.milestone)}")
    lines.append(f"    title: {scalar(args.title)}")
    lines.append("    status: todo")
    if args.priority:
        lines.append(f"    priority: {scalar(args.priority)}")
    deps = [d.strip() for d in (args.depends_on or "").split(",") if d.strip()]
    if deps:
        lines.append("    deps:")
        lines.extend(f"    - {d}" for d in deps)
    else:
        lines.append("    deps: []")
    if args.group:
        lines.append(f"    parallel_group: {scalar(args.group)}")
    use_cases = [u.strip() for u in (args.use_cases or "").split(",") if u.strip()]
    if use_cases:
        lines.append("    use_cases:")
        lines.extend(f"    - {u}" for u in use_cases)
    lines.append("    needs: none")
    if args.model:
        lines.append(f"    model: {scalar(args.model)}")
    if args.effort:
        lines.append(f"    effort: {scalar(args.effort)}")
    if args.acceptance_file:
        acc = Path(args.acceptance_file).read_text(encoding="utf-8").rstrip("\n")
        lines.append("    acceptance: |")
        lines.extend(_prefixed(acc))
    notes_body = []
    if args.found_by:
        notes_body.append(f"FOUND BY {args.found_by}.")
    if args.notes_file:
        notes_body.extend(Path(args.notes_file).read_text(encoding="utf-8").rstrip("\n").splitlines())
    if notes_body:
        lines.append("    notes: |")
        lines.extend(_prefixed("\n".join(notes_body)))
    return "\n".join(lines)


def cmd_new(args: argparse.Namespace) -> int:
    original = args.file.read_text(encoding="utf-8")
    tid = next_id(original)
    block = _build_new_block(tid, args)

    header, blocks, order, trailer = split(original)
    if not order:
        return die("no task list found in the board (no `  - id:` line)")
    blocks[tid] = block
    order.append(tid)
    new_text = reassemble(header, blocks, order, trailer)
    write_checked(args.file, original, new_text)
    print(tid)
    return 0


# --------------------------------------------------------------------------------------------
# validate
# --------------------------------------------------------------------------------------------


def cmd_order(args: argparse.Namespace) -> int:
    """Read-only: which open tickets hold the most work behind them (py/hkpy/taskorder.py)."""
    from hkpy import taskorder
    try:
        doc = load_doc(args.file.read_text(encoding="utf-8"))
    except yaml.YAMLError as e:
        return die(f"docs/tasks.yaml is not strict YAML:\n{e}")
    a = taskorder.analyse(doc.get("tasks") or [])
    if args.json:
        print(json.dumps(a, indent=1))
    else:
        print("\n".join(taskorder.render(a, top=args.top, show_order=args.topo)))
    return 0


def cmd_validate(args: argparse.Namespace) -> int:
    text = args.file.read_text(encoding="utf-8")
    try:
        doc = load_doc(text)
    except yaml.YAMLError as e:
        return die(f"docs/tasks.yaml is not strict YAML:\n{e}")

    tasks = doc.get("tasks") or []
    if len(tasks) < 1:
        return die("no tasks found — the parse is broken, not the board")

    problems: list[str] = []
    ids = [str(t.get("id")) for t in tasks]
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    if dupes:
        problems.append(f"duplicate ids: {dupes}")

    for t in tasks:
        tid = t.get("id")
        status = t.get("status")
        if status not in STATUS_VOCAB:
            problems.append(f"{tid}: status {status!r} is outside the board's vocabulary {sorted(STATUS_VOCAB)}")
        if status == "blocked" and not t.get("blocked_on"):
            problems.append(f"{tid}: blocked with no blocked_on")
        if t.get("blocked_on") and status != "blocked":
            problems.append(f"{tid}: blocked_on set but status is {status!r}")

    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 1
    print(f"task: {len(tasks)} tickets OK")
    return 0


# --------------------------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(
        prog="task",
        description="Edit docs/tasks.yaml through text-level operations on one ticket's block. "
        "The only sanctioned way to edit the board — a hook bans direct edits in worktrees.",
    )
    sub = ap.add_subparsers(dest="cmd", required=True)

    def add_file_arg(p: argparse.ArgumentParser) -> None:
        p.add_argument("--file", type=Path, default=None, help="board file (default docs/tasks.yaml)")

    def add_board_arg(p: argparse.ArgumentParser) -> None:
        # `result`/`note` already spend `--file` on the body text (per spec), so the board path
        # is `--board` for these two subcommands only.
        p.add_argument("--file", type=Path, default=None, help="board file (default docs/tasks.yaml)")

    p = sub.add_parser("show", help="print a ticket's raw block")
    p.add_argument("id")
    add_file_arg(p)
    p.set_defaults(func=cmd_show)

    p = sub.add_parser("list", help="one line per ticket")
    p.add_argument("--status")
    p.add_argument("--milestone")
    p.add_argument("--group")
    p.add_argument("--ready", action="store_true")
    p.add_argument("--json", action="store_true")
    add_file_arg(p)
    p.set_defaults(func=cmd_list)

    p = sub.add_parser("set", help="replace or add fields on a ticket (key=[a, b] writes a list)")
    p.add_argument("id")
    p.add_argument("assignments", nargs="+", metavar="key=value")
    add_file_arg(p)
    p.set_defaults(func=cmd_set)

    p = sub.add_parser("unset", help="remove fields (and their continuation lines) from a ticket")
    p.add_argument("id")
    p.add_argument("keys", nargs="+", metavar="key")
    add_file_arg(p)
    p.set_defaults(func=cmd_unset)

    p = sub.add_parser("result", help="set the `result:` block (replaces any existing one)")
    p.add_argument("id")
    p.add_argument("--from", dest="file_arg", help="read the result text from this file")
    p.add_argument("--text", help="the result text, inline")
    add_board_arg(p)
    p.set_defaults(func=cmd_result)

    p = sub.add_parser("note", help="append to the `notes:` block (creates it if absent)")
    p.add_argument("id")
    p.add_argument("--from", dest="file_arg", help="read the note text from this file")
    p.add_argument("--text", help="the note text, inline")
    add_board_arg(p)
    p.set_defaults(func=cmd_note)

    p = sub.add_parser("new", help="file a new ticket, allocating its id")
    p.add_argument("--title", required=True)
    p.add_argument("--milestone", required=True)
    p.add_argument("--priority")
    p.add_argument("--model")
    p.add_argument("--effort")
    p.add_argument("--group")
    p.add_argument("--depends-on", help="comma-separated T-nnn list")
    p.add_argument("--use-cases", help="comma-separated use-case id list")
    p.add_argument("--acceptance-file")
    p.add_argument("--notes-file")
    p.add_argument("--found-by", help="a ticket/agent id, recorded as the notes' provenance line")
    add_file_arg(p)
    p.set_defaults(func=cmd_new)

    p = sub.add_parser("order", help="read-only: bottlenecks by 'unblocks N', the frontier, a topological order")
    p.add_argument("--top", type=int, default=10)
    p.add_argument("--topo", action="store_true", help="also print the full topological order")
    p.add_argument("--json", action="store_true")
    add_file_arg(p)
    p.set_defaults(func=cmd_order)

    p = sub.add_parser("validate", help="strict-parse the board and check its invariants")
    add_file_arg(p)
    p.set_defaults(func=cmd_validate)

    return ap


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.file is None:
        args.file = default_board()
    if not args.file.exists():
        return die(f"no such file: {args.file}")
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
