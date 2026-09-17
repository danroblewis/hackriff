"""`just gate` — the one diff-aware merge gate (T-396).

Looks at what actually changed, **classifies** it, prints the decision, and runs exactly
the suites that class needs. The "which tests do I run" rule lives here, in the runner,
rather than in each agent's judgement, so the same change gets the same gate whoever ran it.

The rule that makes this safe is that classification **fails closed**: a path that matches
no known class runs the **full** gate, never the cheapest one. That is the repo's own
principle applied to its own tooling — `BiasTee::Unknown` is not `Off`, `Coverage::Unobserved`
is not quiet, `Encryption::Unknown` is not clear. Nothing said is never permissive. A new
top-level directory nobody thought to classify must not silently inherit the ui-only
treatment; it gets the expensive path until someone classifies it on purpose.

Three classes a naive version gets wrong, each deliberate here:

* `fixtures/` is **not** ui/docs-ish — it is acceptance *input*, and the suites read fixture
  metadata (T-317, T-373, T-382 all changed it).
* the `justfile` and `.github/` are **the gate**. A change to them must be verified by the
  expensive path or the gate could weaken itself and certify its own weakening.
* this file and its tests are the gate too, for the same reason — so they are full, not the
  `py/` class they would otherwise land in.

`plugins/`, `spikes/`, `tests/`, `.config/`, `recipes/`, `Cargo.*` and repo-root files are
unclassified and therefore full.

The decision is always printed before anything runs. A silent classifier is a worse version
of the judgement it replaces: an agent or a human has to be able to see the choice and
challenge it, which means seeing the deciding files, not just the verdict.

Stdlib only, so it can run as `python3 py/hkpy/gate.py` as well as `just gate`.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass

# ---------------------------------------------------------------------------
# Classes
# ---------------------------------------------------------------------------

FULL = "full"
UI = "ui"
DOCS = "docs"
PY = "py"

#: Classes in the order they are reported when a change spans more than one.
CLASS_ORDER = (FULL, UI, DOCS, PY)

#: Phases. CI runs the two separately (two jobs, one recipe call each — T-358); a local
#: `just gate` runs both. The classification is identical either way: the phase only says
#: which half of the chosen suites this process runs.
PHASE_CHECK = "check"
PHASE_ACCEPTANCE = "acceptance"
PHASE_ALL = "all"
PHASES = (PHASE_ALL, PHASE_CHECK, PHASE_ACCEPTANCE)

#: What each class runs, per phase. `full` deliberately uses `acceptance-ci` (the census +
#: M0 slice + harness targets) rather than bare `acceptance`, so the gate an agent runs
#: locally and the gate CI runs are one definition rather than two similar ones.
SUITES: dict[str, dict[str, tuple[tuple[str, ...], ...]]] = {
    FULL: {
        PHASE_CHECK: (("just", "lint"), ("just", "test")),
        PHASE_ACCEPTANCE: (("just", "acceptance-ci"),),
    },
    UI: {
        # `just test-ui` is npm ci + build + `tsc --noEmit` + the ui/test suites. The
        # repo's `just lint` is lint-rust + lint-py and there is no JS/TS linter here, so
        # the UI's lint equivalent is the typecheck already inside test-ui; running clippy
        # over the workspace for a change to a .ts file proves nothing it could break.
        PHASE_CHECK: (("just", "test-ui"),),
        PHASE_ACCEPTANCE: (),
    },
    DOCS: {
        # No link checker and no markdown linter exist in this repo, and adding a
        # dependency to satisfy a word in a ticket is the wrong trade. Docs-only is
        # honestly a no-op today; the gate says so out loud rather than implying a check.
        PHASE_CHECK: (),
        PHASE_ACCEPTANCE: (),
    },
    PY: {
        PHASE_CHECK: (("just", "lint-py"), ("just", "test-py")),
        PHASE_ACCEPTANCE: (),
    },
}

#: Canonical ordering for the union of several classes' suites.
_COMMAND_ORDER = (
    ("just", "lint"),
    ("just", "lint-py"),
    ("just", "test"),
    ("just", "test-ui"),
    ("just", "test-py"),
    ("just", "acceptance-ci"),
)

_GATE_SELF = "the gate itself — it must not be able to weaken itself"

#: Ordered rules: (kind, pattern, class, reason). First match wins. `prefix` matches a
#: directory prefix, `exact` a whole path. Anything falling off the end is FULL.
_RULES: tuple[tuple[str, str, str, str], ...] = (
    ("exact", "justfile", FULL, _GATE_SELF + " (justfile)"),
    ("prefix", ".github/", FULL, _GATE_SELF + " (CI workflow)"),
    ("exact", "py/hkpy/gate.py", FULL, _GATE_SELF + " (classifier)"),
    ("exact", "py/tests/test_gate.py", FULL, _GATE_SELF + " (classifier tests)"),
    ("prefix", "crates/", FULL, "Rust workspace — the signal path"),
    ("exact", "docs/api.md", FULL, "the client/server contract (T-079)"),
    ("prefix", "fixtures/", FULL, "acceptance input — the suites read fixture metadata"),
    ("prefix", "tests/", FULL, "the hk-e2e acceptance crate"),
    ("prefix", "plugins/", FULL, "decoder manifests/wrappers the plugin host loads"),
    ("prefix", "recipes/", FULL, "pipeline recipes the suites load"),
    ("prefix", ".config/", FULL, "nextest serial groups — how the suite runs"),
    ("exact", "Cargo.toml", FULL, "workspace manifest"),
    ("exact", "Cargo.lock", FULL, "workspace dependency lock"),
    ("prefix", "ui/", UI, "web client — thin presentation layer"),
    ("prefix", "docs/", DOCS, "documentation"),
    ("prefix", "py/", PY, "Python tooling (orchestration/research only)"),
)

_UNCLASSIFIED = "unclassified path — the gate fails closed"


def normalize(path: str) -> str:
    """Repo-relative, forward slashes, no `./` prefix."""
    p = path.strip().replace("\\", "/")
    while p.startswith("./"):
        p = p[2:]
    return p.strip("/") if p.endswith("/") else p


def classify_path(path: str) -> tuple[str, str]:
    """Classify one changed path. Returns `(class, reason)`.

    Unknown paths are FULL. This is the fail-closed half of the whole design: adding a
    top-level directory must cost the expensive gate until someone adds a rule above.
    """
    p = normalize(path)
    for kind, pattern, klass, reason in _RULES:
        if kind == "exact" and p == pattern:
            return klass, reason
        if kind == "prefix" and p.startswith(pattern):
            return klass, reason
    return FULL, _UNCLASSIFIED


@dataclass(frozen=True)
class Decision:
    """What the gate decided, and why."""

    #: One of the classes, or several joined by `+` (e.g. `ui+docs`), or `empty`.
    label: str
    #: The classes present, in CLASS_ORDER.
    classes: tuple[str, ...]
    #: `(path, class, reason)` for every changed file, sorted by path.
    files: tuple[tuple[str, str, str], ...]
    #: Set when the gate could not work out what changed and forced the full gate.
    forced_reason: str | None = None

    @property
    def is_full(self) -> bool:
        return FULL in self.classes

    @property
    def is_empty(self) -> bool:
        return not self.classes

    def deciding(self) -> tuple[tuple[str, str, str], ...]:
        """The files that decided the class.

        For a full gate those are the files that forced it — the interesting subset when
        one `fixtures/` edit drags 200 doc files up with it. Otherwise, all of them.
        """
        if self.is_full:
            return tuple(f for f in self.files if f[1] == FULL)
        return self.files

    def commands(self, phase: str = PHASE_ALL) -> list[list[str]]:
        """The suites to run, deduped and in canonical order."""
        wanted = (
            (PHASE_CHECK, PHASE_ACCEPTANCE) if phase == PHASE_ALL else (phase,)
        )
        chosen: set[tuple[str, ...]] = set()
        for klass in self.classes:
            for ph in wanted:
                chosen.update(SUITES[klass][ph])
        ordered = [cmd for cmd in _COMMAND_ORDER if cmd in chosen]
        # Defensive: a suite added to SUITES but not to _COMMAND_ORDER still runs.
        ordered += [cmd for cmd in sorted(chosen) if cmd not in _COMMAND_ORDER]
        return [list(cmd) for cmd in ordered]


def classify(paths) -> Decision:
    """Classify a set of changed paths. Pure: no git, no environment, no side effects."""
    seen: dict[str, tuple[str, str]] = {}
    for raw in paths:
        p = normalize(raw)
        if not p:
            continue
        seen[p] = classify_path(p)
    if not seen:
        return Decision(label="empty", classes=(), files=())
    files = tuple(sorted((p, k, r) for p, (k, r) in seen.items()))
    present = {k for _, k, _ in files}
    if FULL in present:
        return Decision(label=FULL, classes=(FULL,), files=files)
    classes = tuple(k for k in CLASS_ORDER if k in present)
    return Decision(label="+".join(classes), classes=classes, files=files)


def forced_full(reason: str) -> Decision:
    """The full gate, chosen because the gate could not tell what changed."""
    return Decision(label=FULL, classes=(FULL,), files=(), forced_reason=reason)


# ---------------------------------------------------------------------------
# Working out what changed
# ---------------------------------------------------------------------------


def _git(args: list[str], root: str) -> tuple[int, str]:
    proc = subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=False
    )
    return proc.returncode, proc.stdout


def _git_ok(args: list[str], root: str) -> str | None:
    rc, out = _git(args, root)
    return out if rc == 0 else None


def _split_z(out: str) -> list[str]:
    return [t for t in out.split("\0") if t]


def committed_changes(root: str, base_ref: str) -> list[str] | None:
    """Paths changed since the **merge base** with `base_ref`.

    Merge base, not `base_ref` itself: diffing against a moved main makes every file
    someone else touched look like part of this branch, which would push every branch to
    the full gate for no reason (and, worse, teach people to distrust the classifier).
    `--no-renames` so a rename counts as touching both the old and the new path, and
    deletions count as touching the path.
    """
    base = _git_ok(["merge-base", base_ref, "HEAD"], root)
    if base is None:
        return None
    base = base.strip()
    out = _git_ok(["diff", "--name-only", "-z", "--no-renames", base, "HEAD"], root)
    if out is None:
        return None
    return _split_z(out)


def staged_changes(root: str) -> list[str] | None:
    out = _git_ok(["diff", "--cached", "--name-only", "-z", "--no-renames"], root)
    return None if out is None else _split_z(out)


def worktree_changes(root: str) -> list[str] | None:
    """Everything uncommitted: staged, unstaged, and untracked.

    Untracked files count. A brand-new `newdir/thing.rs` that nobody has `git add`ed yet is
    still a change the gate must see — leaving it out is the one way this could fail open.
    The cost is that stray untracked scratch files force the full gate; they are printed as
    deciding files, so that is visible and answerable rather than mysterious.
    """
    out = _git_ok(
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"], root
    )
    if out is None:
        return None
    tokens = out.split("\0")
    paths: list[str] = []
    i = 0
    while i < len(tokens):
        entry = tokens[i]
        i += 1
        if not entry:
            continue
        status, _, path = entry[:2], entry[2:3], entry[3:]
        if path:
            paths.append(path)
        # Renames/copies emit `XY new\0orig\0`: both paths were touched.
        if status[0] in "RC" or status[1] in "RC":
            if i < len(tokens) and tokens[i]:
                paths.append(tokens[i])
            i += 1
    return paths


@dataclass(frozen=True)
class Source:
    """Where the changed-file list came from, for printing."""

    description: str
    paths: list[str] | None
    forced: str | None = None


def resolve_source(args, root: str) -> Source:
    """Pick the diff to classify, and say so.

    Explicit wins: `--files`, `--staged`, `--base`, `--worktree`. Otherwise:

    * In GitHub Actions on a pull request, the PR base (`origin/$GITHUB_BASE_REF`).
    * In GitHub Actions on a push, **the full gate**. There is no base to compare against
      that is worth trusting, and main is the branch everything else is measured from, so
      it gets verified whole. PRs get the cheap classified gate; main never does.
    * Locally, the merge base with `main` **plus** everything uncommitted — which on `main`
      itself degenerates to just the uncommitted set, exactly as it should.
    """
    if args.files is not None:
        return Source("explicit --files", list(args.files))

    if args.staged:
        return Source("staged changes", staged_changes(root))

    if args.base:
        paths = committed_changes(root, args.base)
        if paths is None:
            return Source(
                f"merge base with {args.base}",
                None,
                forced=f"cannot resolve a merge base with {args.base!r}",
            )
        return Source(f"merge base with {args.base}", paths)

    if args.worktree:
        return Source("uncommitted (staged + unstaged + untracked)", worktree_changes(root))

    if os.environ.get("GITHUB_ACTIONS") == "true":
        base_ref = os.environ.get("GITHUB_BASE_REF", "").strip()
        if not base_ref:
            return Source(
                "CI push build",
                None,
                forced="CI push build: no pull-request base, so the full gate runs",
            )
        remote_ref = f"origin/{base_ref}"
        paths = committed_changes(root, remote_ref)
        if paths is None:
            return Source(
                f"CI pull request, merge base with {remote_ref}",
                None,
                forced=(
                    f"cannot resolve {remote_ref!r} — the checkout needs fetch-depth: 0"
                ),
            )
        return Source(f"CI pull request, merge base with {remote_ref}", paths)

    default_branch = args.default_branch
    committed = committed_changes(root, default_branch)
    if committed is None:
        return Source(
            f"merge base with {default_branch} + uncommitted",
            None,
            forced=f"cannot resolve a merge base with {default_branch!r}",
        )
    uncommitted = worktree_changes(root)
    if uncommitted is None:
        return Source(
            f"merge base with {default_branch} + uncommitted",
            None,
            forced="cannot read the working tree status",
        )
    return Source(
        f"merge base with {default_branch} + uncommitted", committed + uncommitted
    )


# ---------------------------------------------------------------------------
# Printing and running
# ---------------------------------------------------------------------------

_MAX_PRINTED = 25


def render(decision: Decision, source: Source, phase: str) -> list[str]:
    """The decision, as printed lines. Always printed, before anything runs."""
    out = [f"gate: source   = {source.description}"]
    if decision.forced_reason:
        out.append(f"gate: FULL     = {decision.forced_reason}")
    else:
        out.append(f"gate: changed  = {len(decision.files)} file(s)")
    out.append(f"gate: class    = {decision.label}")

    deciding = decision.deciding()
    if deciding:
        what = "deciding files" if not decision.is_full else "files forcing the full gate"
        out.append(f"gate: {what}:")
        for path, klass, reason in deciding[:_MAX_PRINTED]:
            out.append(f"gate:   {path}  [{klass}] {reason}")
        if len(deciding) > _MAX_PRINTED:
            out.append(f"gate:   ... and {len(deciding) - _MAX_PRINTED} more")

    commands = decision.commands(phase)
    phase_note = "" if phase == PHASE_ALL else f" (phase: {phase})"
    if decision.is_empty:
        out.append(f"gate: suites   = none — nothing changed{phase_note}")
    elif commands:
        out.append(
            "gate: suites   = " + "; ".join(" ".join(c) for c in commands) + phase_note
        )
    else:
        out.append(f"gate: suites   = none for this class{phase_note}")
        if DOCS in decision.classes:
            out.append(
                "gate:            docs-only: this repo has no link checker and no "
                "markdown linter, so there is nothing to run."
            )
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just gate",
        description=(
            "Classify the current diff and run exactly the suites it needs. "
            "Unclassified paths run the full gate."
        ),
    )
    src = parser.add_argument_group("what to classify (default: merge base with main + uncommitted)")
    src.add_argument("--base", metavar="REF", help="diff against the merge base with REF")
    src.add_argument("--staged", action="store_true", help="classify the staged set only")
    src.add_argument(
        "--worktree", action="store_true", help="classify uncommitted changes only"
    )
    src.add_argument(
        "--files", nargs="*", metavar="PATH", help="classify this explicit path list"
    )
    src.add_argument("--default-branch", default="main", help="default: main")
    parser.add_argument(
        "--phase",
        choices=PHASES,
        default=PHASE_ALL,
        help="run only half the suites: `check` (lint/test) or `acceptance`",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the decision and the suites, run nothing",
    )
    parser.add_argument(
        "--root", default=None, help="repo root (default: the git toplevel)"
    )
    args = parser.parse_args(argv)

    root = args.root
    if root is None:
        rc, out = _git(["rev-parse", "--show-toplevel"], os.getcwd())
        root = out.strip() if rc == 0 and out.strip() else os.getcwd()

    source = resolve_source(args, root)
    if source.forced or source.paths is None:
        decision = forced_full(source.forced or "could not read the diff")
    else:
        decision = classify(source.paths)

    for line in render(decision, source, args.phase):
        print(line, flush=True)

    commands = decision.commands(args.phase)
    if args.dry_run or not commands:
        return 0

    if shutil.which("just") is None:
        print("gate: `just` is not on PATH — cannot run the suites.", file=sys.stderr)
        return 1

    for cmd in commands:
        print(f"gate: running {' '.join(cmd)}", flush=True)
        rc = subprocess.run(cmd, cwd=root, check=False).returncode
        if rc != 0:
            print(f"gate: FAILED {' '.join(cmd)} (exit {rc})", file=sys.stderr)
            return rc
    print(f"gate: passed ({decision.label})", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
