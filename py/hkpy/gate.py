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

**Two subjects, two sources (T-424).** The default source — merge base with `main` *plus*
everything uncommitted, untracked included — answers *"what is in my tree that isn't on
main?"*. That is the right question for an **agent** checking its own work: an untracked
`newdir/thing.rs` it has not `git add`ed yet is still going to be committed, so it is part
of the change and must be classified. The **coordinator's per-merge gate asks a different
question** — *"what will this merge put on main?"* — and `--merge` answers exactly that by
classifying the **index during an in-progress merge**, which git itself built as the merge
result versus HEAD. Untracked files are, by construction, not in that index and cannot reach
main through that merge; the permanently-untracked `tools/` and diagnostic captures in the
coordinator's checkout were degrading every merge gate to full for files that can never be
part of any merge.

That narrowing is guarded, not assumed:

* `--merge` **requires an in-progress merge** (`MERGE_HEAD`, or `SQUASH_MSG` for a squash).
  Without one there is no guarantee the index is a merge result, so it **forces the full
  gate** rather than classifying whatever happens to be staged. Fail-closed, as everywhere.
* Every uncommitted path it did *not* classify is **printed**, with the class it would have
  had. Nothing is ignored silently; there is no ignore list, and `fixtures/` staged in a
  merge is still full.

The counterexample worth stating: an untracked fixture *can* change what a suite sees when
that suite runs. Classifying untracked paths as FULL never protected against that — the
suites read the working tree, so a stray file changes their result at whatever class was
chosen. Fail-closed on untracked paths buys **cost, not safety**, for the merge subject. It
buys real safety for the agent subject, where untracked means not-yet-added-but-will-land,
which is why the default keeps it.

**The suites this file launches build with T-144's flags (T-400).** Every agent brief sets
`CARGO_INCREMENTAL=0` (and the rest of T-144) so worktree targets stay close to the APFS clone
they were seeded from, but `main()` used to shell out to `just lint`/`just test`/etc. without
them, so the coordinator's own gate regenerated `target/debug/incremental` on every run — state
every agent is configured to avoid. `GATE_BUILD_ENV`/`suite_env()` fix that at the one place
this file spawns a suite, not in the justfile: a developer's own `just test` still gets whatever
incremental behaviour they want.

**The gate diagnoses its own cost (`hkpy.gatediag`).** After each suite it compares every test
to its own median over the last green runs of the same suite, aggregates by crate, and splits
the crates by whether *this diff* touched them. Untouched crates over 3x are contention and say
nothing about the change (the 09-21 gate: hk-recipe 79x, hk-dsp 20x, hk-core 18x, while the 8
tests the diff added cost 0 s); touched crates over 3x are the change's own cost. The verdict
goes in the `gate_end` record and, when contended, into one amber alert. It is wrapped whole:
a stopwatch with an opinion must still never fail the thing it times.

Stdlib only, so it can run as `python3 py/hkpy/gate.py` as well as `just gate`.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import time
from dataclasses import dataclass

try:  # `python -m hkpy.gate` (how `just gate` runs it)
    from . import crates as crate_select
    from . import gatediag, gatelog
except ImportError:  # `python3 py/hkpy/gate.py`, the direct-execution path the docstring promises
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from hkpy import crates as crate_select  # type: ignore[no-redef]
    from hkpy import gatediag, gatelog  # type: ignore[no-redef]

# ---------------------------------------------------------------------------
# Classes
# ---------------------------------------------------------------------------

FULL = "full"
UI = "ui"
DOCS = "docs"
PY = "py"

#: Classes in the order they are reported when a change spans more than one.
OPS = "ops"
CLASS_ORDER = (FULL, UI, DOCS, PY, OPS)

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
    # Orchestration: ops/ scripts, .claude/ roles-agents-skills-hooks, prompts/. None of it is
    # linked into a crate or read by a suite, so the full workspace run proves nothing about it -
    # and on 2026-09-22 four docs/ops-only branches each burned a 50-minute full gate and lost it
    # to a load-sensitive Rust test they could not have touched. What CAN break here is a bash
    # script or a Python tool, so: syntax-check the scripts and run the Python suite (which also
    # validates the board).
    OPS: {
        PHASE_CHECK: (("just", "ops-check"), ("just", "lint-py"), ("just", "test-py")),
        PHASE_ACCEPTANCE: (),
    },
    FULL: {
        PHASE_CHECK: (("just", "lint"), ("just", "test")),
        PHASE_ACCEPTANCE: (("just", "acceptance-ci"), ("just", "test-ui-e2e")),
    },
    UI: {
        # `just test-ui` is npm ci + build + `tsc --noEmit` + the ui/test suites. The
        # repo's `just lint` is lint-rust + lint-py and there is no JS/TS linter here, so
        # the UI's lint equivalent is the typecheck already inside test-ui; running clippy
        # over the workspace for a change to a .ts file proves nothing it could break.
        PHASE_CHECK: (("just", "test-ui"),),
        # `just test-ui-e2e` (T-455) is the browser tier: headless Chrome over a real
        # `hk serve`, asserting on what /surface draws and what it requests. It is in the
        # ACCEPTANCE phase, not CHECK, because it needs the `hk` binary and a browser
        # while `test-ui` needs neither — putting it in CHECK would make every `.ts` typo
        # pay for a Rust build. It runs for the `full` class too: this is the only suite
        # that notices when a backend change breaks the page that consumes it.
        #
        # It is not optional, and that is the point of the ticket that added it. Two
        # defects in two days (T-450's CSP throw at module scope, T-454's 503 reaching the
        # user) passed every suite this repo had, and a verification tier nobody is
        # obliged to run is worse than none, because it looks like coverage.
        PHASE_ACCEPTANCE: (("just", "test-ui-e2e"),),
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
    ("just", "test-ui-e2e"),
)

#: T-400: the T-144 build flags, applied only to the suite subprocesses the gate itself
#: launches — never written into the justfile globally, because a developer running `just
#: build`/`just test`/`just lint` by hand may want incremental compilation. This is what "the
#: rule lives in the runner" means here, exactly as T-396 moved the which-suites rule into
#: this file instead of each agent's judgement.
#:
#: Every T-144 flag is included, each for its own reason — not copied by reflex:
#:
#:   CARGO_INCREMENTAL=0                        the ticket's own trigger. Without it, every
#:                                               `just gate` regenerates target/debug/incremental
#:                                               in the coordinator's OWN checkout — 3.6 GB in one
#:                                               session — diverging it from the clean state every
#:                                               future worktree clones via `cp -c -R -p`.
#:   CARGO_PROFILE_DEV_DEBUG=line-tables-only    every worktree already builds with this (T-144).
#:                                               If the gate's own suites built full debug info
#:                                               instead, the coordinator's target/ would carry a
#:                                               different profile fingerprint than what worktrees
#:                                               build against, which is its own source of
#:                                               unwanted rebuild divergence — the same failure
#:                                               mode this ticket exists to close, one layer up.
#:   CARGO_BUILD_JOBS=6                          CLAUDE.md's Coordination section counts "the
#:                                               coordinator's full check" as one of the "at most
#:                                               4 Rust-building agents" T-144 caps at 6 jobs each
#:                                               (4 x 6 ~= 28 cores). Left unbounded, a `just gate`
#:                                               run would oversubscribe on top of up to three
#:                                               concurrently building worktrees exactly as an
#:                                               agent that skipped the cap would.
#:
#: nextest's test-threads is NOT set here - `.config/nextest.toml` (T-436) is its one home - but an
#: INHERITED `NEXTEST_TEST_THREADS` is removed, because that variable beats the profile. On
#: 2026-09-23 02:11 `.claude/settings.json` gave every Claude session NEXTEST_TEST_THREADS=2 (a
#: per-session build bound, f2fc783b); the merge runner restarted from a session at 04:05 inherited
#: it, and from the 04:14 gate on every `just test` ran at a measured concurrency of exactly 2.0
#: (JUnit: ~3300 test-seconds in ~1650 s wall, against 4.5-7 and ~780 s before) - about +870 s per
#: full gate, all day, with no line anywhere saying so. The only way to change the gate's thread
#: count is now the knob store (`just knobs set NEXTEST_TEST_THREADS=N`), which is deliberate and
#: logged; a session's ambient bound never reaches the gate.
GATE_BUILD_ENV: dict[str, str] = {
    "CARGO_INCREMENTAL": "0",
    "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
    "CARGO_BUILD_JOBS": "6",
}


def suite_env(base: dict[str, str], store: dict[str, str] | None = None) -> dict[str, str]:
    """`base` (normally `os.environ`) with `GATE_BUILD_ENV` applied on top, and the test-thread
    count taken from the knob `store` or else left to the nextest profile - never inherited.

    A pure function of its inputs so it's testable without touching the real environment or
    spawning anything: it adds exactly the T-144 flags, drops an inherited NEXTEST_TEST_THREADS,
    and changes nothing else.
    """
    env = {**base, **GATE_BUILD_ENV}
    env.pop("NEXTEST_TEST_THREADS", None)
    if (store or {}).get("NEXTEST_TEST_THREADS"):
        env["NEXTEST_TEST_THREADS"] = store["NEXTEST_TEST_THREADS"]
    return env


def knob_store() -> dict[str, str]:
    """`$HACKRIFF_OPS/env` (`just knobs`) as a dict; empty when absent (CI, a fresh box)."""
    path = os.path.join(os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops"), "env")
    out: dict[str, str] = {}
    try:
        for ln in open(path, encoding="utf-8"):
            k, sep, v = ln.strip().partition("=")
            if sep and k and not k.startswith("#"):
                out[k.strip()] = v.strip()
    except OSError:
        pass
    return out


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
    ("prefix", ".config/", FULL, "nextest thread cap + serial groups — how the suite runs"),
    ("exact", "Cargo.toml", FULL, "workspace manifest"),
    ("exact", "Cargo.lock", FULL, "workspace dependency lock"),
    ("prefix", "ui/", UI, "web client — thin presentation layer"),
    # T-561: the board is DATA the tooling parses, not prose. A malformed `blocked_on:`
    # (an unquoted value containing ": ") once reached main because docs/ runs NOTHING,
    # so py/tests/test_task_board.py - which exists to catch exactly that - never ran,
    # and it broke the board test, the dashboard and every gate that parses the file.
    ("exact", "docs/tasks.yaml", PY, "the board - the Python suite parses and validates it"),
    ("exact", "docs/use-cases.yaml", PY, "machine-readable use cases - the Python suite reads it"),
    ("prefix", "docs/", DOCS, "documentation"),
    ("prefix", "py/", PY, "Python tooling (orchestration/research only)"),
    ("prefix", "ops/", OPS, "orchestration scripts - not linked into any crate"),
    ("prefix", ".claude/", OPS, "roles, agents, skills, hooks - prompts and hook scripts"),
    ("prefix", "prompts/", OPS, "model-selection and briefing prompts"),
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


#: The sub-steps of a suite that a red in an EARLIER sub-step leaves unrun, and so the only ones
#: `--resume-steps` may name. The workspace-test recipe runs `test-rust` LAST (a Rust red never
#: skips a sibling); `acceptance-ci` runs `acceptance` then `e2e-harness`, both nextest.
RESUMABLE_STEPS: dict[str, tuple[str, ...]] = {
    "acceptance-ci": ("e2e-harness",),
}


def resume(commands: list[list[str]], after: str, steps: list[str] | tuple[str, ...] = ()) -> list[list[str]]:
    """The suites to run when a gate resumes after `after` (pure).

    Only a SUFFIX of the classified list, so a resume can never skip a suite that did not run:
    `after` must be one of `commands`, and each of `steps` must be a known unrun sub-step of it.
    """
    names = [c[1] for c in commands if len(c) == 2 and c[0] == "just"]
    if after not in names:
        raise ValueError(f"refusing --resume-after {after!r}: not a suite of this gate ({', '.join(names) or 'none'})")
    bad = [x for x in steps if x not in RESUMABLE_STEPS.get(after, ())]
    if bad:
        raise ValueError(f"refusing --resume-steps {' '.join(bad)}: not an unrun sub-step of {after!r}")
    i = names.index(after)
    return [["just", x] for x in steps] + [list(c) for c in commands[i + 1:]]


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


def merge_state(root: str) -> str | None:
    """Name the in-progress merge, or `None` if there isn't one.

    `git merge --no-ff --no-commit` leaves `MERGE_HEAD`; `git merge --squash` leaves
    `SQUASH_MSG` and no `MERGE_HEAD`. Either one means **git** built the index from a merge,
    which is the whole precondition `--merge` rests on: the index is then the merge result
    versus HEAD, not an arbitrary pile of `git add`s.
    """
    if _git_ok(["rev-parse", "-q", "--verify", "MERGE_HEAD"], root) is not None:
        return "MERGE_HEAD"
    path = _git_ok(["rev-parse", "--git-path", "SQUASH_MSG"], root)
    if path and os.path.exists(os.path.join(root, path.strip())):
        return "SQUASH_MSG"
    return None


#: What `--merge` classifies, said out loud in the printed decision.
MERGE_SUBJECT = "merge index vs HEAD — exactly what this merge puts on main"

#: The misuse guard. `--merge` outside a merge cannot justify its own narrowing, so it
#: fails closed to the full gate rather than classifying whatever happens to be staged.
NOT_A_MERGE = (
    "--merge outside an in-progress merge: nothing guarantees the index is a merge result, "
    "so the narrowing is unjustified and the full gate runs"
)


@dataclass(frozen=True)
class Source:
    """Where the changed-file list came from, for printing."""

    description: str
    paths: list[str] | None
    forced: str | None = None
    #: Uncommitted paths deliberately left out of the classified set (`--merge` only).
    #: Printed with the class they would have had, so the narrowing is visible, never silent.
    outside: tuple[str, ...] = ()


def merge_source(
    staged: list[str] | None, uncommitted: list[str] | None, state: str | None
) -> Source:
    """Build the `--merge` Source from three already-gathered git facts. Pure.

    `staged` is `git diff --cached` (the merge result vs HEAD), `uncommitted` is the whole
    working-tree change set including untracked, `state` names the in-progress merge.

    The narrowing happens here and nowhere else, so it is one testable function: classify
    `staged`, and carry everything in `uncommitted` that is not in it as `outside` — printed,
    never classified, never silently dropped.
    """
    if state is None:
        return Source("merge index", None, forced=NOT_A_MERGE)
    if staged is None or uncommitted is None:
        return Source(
            "merge index", None, forced="cannot read the index or the working tree"
        )
    inside = {normalize(p) for p in staged if normalize(p)}
    outside = tuple(sorted({normalize(p) for p in uncommitted if normalize(p)} - inside))
    return Source(f"{MERGE_SUBJECT} [{state}]", list(staged), outside=outside)


def resolve_source(args, root: str) -> Source:
    """Pick the diff to classify, and say so.

    Explicit wins: `--merge`, `--files`, `--staged`, `--base`, `--worktree`. Otherwise:

    * In GitHub Actions on a pull request, the PR base (`origin/$GITHUB_BASE_REF`).
    * In GitHub Actions on a push, **the full gate**. There is no base to compare against
      that is worth trusting, and main is the branch everything else is measured from, so
      it gets verified whole. PRs get the cheap classified gate; main never does.
    * Locally, the merge base with `main` **plus** everything uncommitted — which on `main`
      itself degenerates to just the uncommitted set, exactly as it should.
    """
    if args.merge:
        return merge_source(
            staged_changes(root), worktree_changes(root), merge_state(root)
        )

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

    if source.outside:
        out.append(
            f"gate: outside  = {len(source.outside)} uncommitted path(s) NOT in this merge, "
            "listed with the class they would have had:"
        )
        for path in source.outside[:_MAX_PRINTED]:
            klass, reason = classify_path(path)
            out.append(f"gate:   {path}  [would be {klass}] {reason}")
        if len(source.outside) > _MAX_PRINTED:
            out.append(f"gate:   ... and {len(source.outside) - _MAX_PRINTED} more")
        out.append(
            "gate:            none of these can reach main through this merge. They CAN "
            "change what a suite that does run sees, so judge the run, not the class."
        )

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


#: T-543, corrected 2026-09-20. The one place the crate narrowing reaches the suites: the
#: justfile's `lint-rust`, `test-rust` and `test-doc` read this variable and turn it into
#: `-p` flags. **Unset means the whole workspace**, so the failure mode of every bug in this
#: path — a crash, a missing `cargo`, a typo, an old justfile — is the expensive gate, never
#: a cheap one. That is the same fail-closed shape as `classify_path`'s unknown-is-FULL,
#: moved one level down. It is now also the *default* shape: only `--select-crates` (an
#: agent's own opt-in for local iteration) ever sets it, and `resolve_selection` refuses to
#: set it at all for `--merge` or inside CI regardless of that flag — see its docstring.
CRATES_ENV = "HK_GATE_CRATES"

#: T-543. Set for a `full`-class diff that contains no `ui/` path, so `just test-ui` (npm ci
#: + esbuild + `tsc --noEmit` + the node suites, ~2.5 min every crate gate) does not run over
#: TypeScript the diff did not touch. `ui/` has no generated input — nothing in that pipeline
#: reads a Rust artifact — so a `crates/`-only change cannot change its answer.
#:
#: It skips the CHECK-phase UI suite only. `just test-ui-e2e`, the browser tier that drives a
#: real `hk serve` and is the one suite that notices a backend change breaking the page, runs
#: for the `full` class regardless and is never skipped.
#:
#: Unset means RUN, like `HK_GATE_CRATES`: a bug here costs 2.5 minutes, never coverage.
SKIP_UI_ENV = "HK_GATE_SKIP_UI"

#: What `ops/merge-runner.sh` gave up waiting for before starting this gate: unowned CPU or a
#: load over the box's plan, as `ops/watchdog.py` last saw it. Purely informational - it changes
#: no suite and no decision - but a gate run beside a 100 % process nobody owns is not a
#: measurement of the code, and the gate log is where that has to be said or it is lost.
CONTENDED_ENV = "HK_GATE_CONTENDED"


def skip_ui(decision: Decision, source: Source) -> bool:
    """True when the CHECK-phase UI suite can be skipped for this diff.

    Only for a classified `full` diff whose path list is known and contains no `ui/` path.
    A forced full gate has no path list, so it cannot claim the UI is untouched.
    """
    if not decision.is_full or decision.forced_reason or source.paths is None:
        return False
    return not any(normalize(str(p)).startswith("ui/") for p in source.paths)


def resolve_selection(
    decision: Decision,
    source: Source,
    root: str,
    *,
    select_crates: bool = False,
    no_select: bool = False,
    merge: bool = False,
    ci: bool = False,
) -> "crate_select.Selection":
    """Narrow the Rust suite to the crates a `full`-class diff can reach, or don't.

    T-543 CORRECTED BY THE USER (2026-09-20): affected-crate selection is a convenience for
    an AGENT'S OWN local iteration only. It must never reduce coverage on a path that can
    reach `main` — so this function checks the two paths that can, ``merge`` and ``ci``,
    FIRST and unconditionally, before it even looks at ``select_crates``:

      * ``merge=True`` is `just gate-merge` (T-424) — the coordinator's per-merge gate.
      * ``ci=True`` is `just gate` running inside GitHub Actions.

    Neither is ever narrowed, even if ``select_crates=True`` were somehow also passed — the
    check is unconditional, not "narrowing wasn't requested this time", so a future caller
    cannot recreate T-543's original mistake by wiring the flag into the merge/CI call site
    by accident. `py/tests/test_gate.py` pins this the same way it pins classification's
    fail-closed shape: as a test on the function, not as trust that no caller ever passes it.

    Everywhere else, narrowing is OPT-IN via ``--select-crates``: unset (the default) means
    the whole workspace, the same fail-closed shape as `classify_path`'s unknown-is-FULL.
    ``no_select`` stays as an explicit "definitely don't" for a caller that has its own
    reason to pass `--select-crates` and `--no-select` together in one invocation.
    """
    if merge:
        return crate_select.Selection(None, "the merge gate never narrows crates (T-543, corrected)")
    if ci:
        return crate_select.Selection(None, "the CI gate never narrows crates (T-543, corrected)")
    if no_select or not select_crates:
        return crate_select.Selection(
            None,
            "crate narrowing is opt-in (--select-crates), for an agent's own local "
            "iteration, and was not requested",
        )
    if not decision.is_full:
        return crate_select.Selection(None, "not the full class — no Rust suite to narrow")
    if decision.forced_reason or source.paths is None:
        return crate_select.Selection(
            None, "the changed-path list is unknown — running the whole workspace"
        )
    return crate_select.select(source.paths, crate_select.load_workspace(root))


def render_selection(selection: "crate_select.Selection") -> list[str]:
    """The crate decision, printed like the class decision: never silent."""
    if selection.is_workspace:
        return [f"gate: crates   = WHOLE WORKSPACE ({selection.reason})"]
    return [
        f"gate: crates   = {len(selection.crates or ())} of the workspace: "
        + " ".join(selection.crates or ()),
        f"gate:            {selection.reason}",
    ]


def selection_env(
    base: dict[str, str], selection: "crate_select.Selection"
) -> dict[str, str]:
    """`base` with `HK_GATE_CRATES` set, or explicitly cleared for a workspace run.

    Cleared, not left alone: an inherited `HK_GATE_CRATES` from an outer shell must not
    silently narrow a gate that decided on the whole workspace.
    """
    env = dict(base)
    if selection.is_workspace or not selection.crates:
        env.pop(CRATES_ENV, None)
    else:
        env[CRATES_ENV] = " ".join(selection.crates)
    return env


def current_branch(root: str) -> str | None:
    """The branch being gated, for the cycle-time ledger. `None` on a detached HEAD.

    Never raises: this is a label on a measurement, and a measurement must not be able to
    fail the thing it measures. Same rule as `gatelog.append`.
    """
    try:
        out = _git_ok(["rev-parse", "--abbrev-ref", "HEAD"], root)
    except Exception:
        return None
    if out is None:
        return None
    name = out.strip()
    return None if not name or name == "HEAD" else name


def head_sha(root: str) -> str | None:
    """The commit being gated, short form. Never raises — see `current_branch`."""
    try:
        out = _git_ok(["rev-parse", "--short", "HEAD"], root)
    except Exception:
        return None
    return out.strip() if out and out.strip() else None


def keep_junit(root: str, run_id: str, n: int, cmd: list[str], since: float) -> list[str]:
    """Copy every nextest JUnit report this suite wrote into `$HACKRIFF_OPS/junit/<run_id>/`.

    User, 2026-09-22: a 36-minute gate with no per-test record is unmeasurable — "we should be
    recording whatever the normal machine-readable output is for the test suite". nextest writes
    one `target/nextest/<profile>/junit.xml` per run when `.config/nextest.toml` names a
    `[profile.default.junit] path`, and OVERWRITES it on the next run — `just test` and
    `just acceptance-ci` both run under the default profile — so the gate copies it away after
    each suite, keyed by run id and suite order, before the next suite can clobber it. Every
    `<testcase>` carries its `time`, and the file order is the run order, so a slow gate can be
    read test by test (`ops/monitor.py` renders the newest). Only files written during this
    suite are taken (`since`): a stale report from an earlier run is not this suite's evidence.
    Never fails the gate — a missing report is a missing measurement, not a red.
    """
    import glob

    kept: list[str] = []
    try:
        dest = os.path.join(gatelog.ops_dir(), "junit", run_id)
        for src in sorted(glob.glob(os.path.join(root, "target", "nextest", "*", "junit.xml"))):
            if os.path.getmtime(src) < since:
                continue
            profile = os.path.basename(os.path.dirname(src))
            suite = "-".join(cmd[1:]) or cmd[0]
            os.makedirs(dest, exist_ok=True)
            out = os.path.join(dest, f"{n:02d}-{suite}-{profile}.xml")
            shutil.copyfile(src, out)
            kept.append(out)
    except OSError as e:  # pragma: no cover - best effort by design
        print(f"gate: junit    = not kept ({e})", file=sys.stderr)
    return kept


def diagnose_kept(
    kept: list[str],
    *,
    source: Source,
    cmd: list[str],
    records: list[dict] | None,
    out: list,
) -> None:
    """Print `hkpy.gatediag`'s verdict for every JUnit file this suite just wrote.

    WRAPPED WHOLE, deliberately. This is a stopwatch with an opinion, and a stopwatch must not
    be able to fail the thing it times — the same rule `gatelog.append` is written under. Any
    exception at all degrades to one stderr line and a gate that carries on exactly as before.
    """
    try:
        for path in kept:
            diag = gatediag.diagnose_suite(
                path,
                changed_paths=source.paths,
                records=records,
                cmd=" ".join(cmd),
            )
            if diag is None:
                continue
            out.append(diag)
            print(diag.line(), flush=True)
            dearer = diag.dearer_line()
            if dearer:
                print(dearer, flush=True)
    except Exception as e:  # pragma: no cover - by design; see the docstring
        print(f"gate: timing   = diagnosis unavailable ({e})", file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just gate",
        description=(
            "Classify the current diff and run exactly the suites it needs. "
            "Unclassified paths run the full gate."
        ),
    )
    src = parser.add_argument_group("what to classify (default: merge base with main + uncommitted)")
    src.add_argument(
        "--merge",
        action="store_true",
        help=(
            "the coordinator's per-merge gate: classify the index of an in-progress merge "
            "(what this merge puts on main). Requires a merge in progress, or it runs full."
        ),
    )
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
        "--resume-after",
        metavar="SUITE",
        help=(
            "run only the classified suites AFTER this one (a recipe name, e.g. `test`), plus any "
            "--resume-steps. The merge runner's flake acceptance (user, 2026-09-23): a red test that "
            "passed alone twice is accepted and the gate goes on from where it stopped - it never "
            "re-runs what already ran, and it can only drop suites that ran BEFORE the named one. "
            "A name that is not in this gate's list is refused."
        ),
    )
    parser.add_argument(
        "--resume-steps",
        metavar="RECIPE",
        nargs="*",
        default=[],
        help="sub-steps of the resumed suite that never ran; only those in RESUMABLE_STEPS are accepted",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the decision and the suites, run nothing",
    )
    parser.add_argument(
        "--select-crates",
        action="store_true",
        help=(
            "OPT-IN ONLY (T-543, corrected 2026-09-20): narrow the Rust suite to the crates "
            "this diff can reach, for an agent's own local iteration. Off by default. "
            "Never implied by class, never passed by `just gate-merge` or by CI, and "
            "structurally refused under --merge or inside GITHUB_ACTIONS even if passed — "
            "the merge/CI gate always runs the whole workspace regardless of this flag."
        ),
    )
    parser.add_argument(
        "--no-select",
        action="store_true",
        help=(
            "explicitly disable crate narrowing (default already, since it's opt-in); wins "
            "over --select-crates if both are given."
        ),
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

    # T-543, corrected 2026-09-20: which CRATES, ONLY when `--select-crates` opted in, and
    # NEVER for `--merge` (the coordinator's per-merge gate) or inside GITHUB_ACTIONS (CI) —
    # both checked unconditionally inside resolve_selection itself, not by this call site
    # simply not passing the flag. Purely a narrowing of the Rust suite inside the same
    # class — never a change of class, and never applied when the answer is not certain
    # (`crates.select` returns the whole workspace then). See `crates.py`.
    ci = os.environ.get("GITHUB_ACTIONS") == "true"
    selection = resolve_selection(
        decision,
        source,
        root,
        select_crates=args.select_crates,
        no_select=args.no_select,
        merge=args.merge,
        ci=ci,
    )
    for line in render_selection(selection):
        print(line, flush=True)

    ui_skipped = skip_ui(decision, source)
    if ui_skipped:
        print(
            "gate: ui       = `just test-ui` SKIPPED — no ui/ path in this diff, and ui/ has "
            "no generated input. The browser tier (just test-ui-e2e) still runs.",
            flush=True,
        )

    commands = decision.commands(args.phase)
    if args.resume_after:
        try:
            commands = resume(commands, args.resume_after, args.resume_steps)
        except ValueError as e:
            print(f"gate: {e}", file=sys.stderr)
            return 2
        print(f"gate: resume   = after `just {args.resume_after}`"
              + (f" + {' '.join(args.resume_steps)}" if args.resume_steps else "")
              + f" -> {'; '.join(' '.join(c) for c in commands) or 'nothing left to run'}"
              " (a red test passed alone twice; the suites before it passed in the stopped run)", flush=True)
    if args.dry_run or not commands:
        return 0

    if shutil.which("just") is None:
        print("gate: `just` is not on PATH — cannot run the suites.", file=sys.stderr)
        return 1

    # T-400: the suites the gate itself launches get T-144's build flags (env only — this
    # changes nothing about *which* commands run or what they assert, only how cargo builds
    # while they run). Printed so the override is visible, not a silent side effect.
    env = suite_env(dict(os.environ), knob_store())
    print(
        "gate: build env = "
        + " ".join(f"{k}={v}" for k, v in GATE_BUILD_ENV.items())
        + f" NEXTEST_TEST_THREADS={env.get('NEXTEST_TEST_THREADS', '(profile)')}"
        + (f" (inherited {os.environ['NEXTEST_TEST_THREADS']} dropped)" if "NEXTEST_TEST_THREADS" in os.environ else "")
        + " (T-400, this process only)",
        flush=True,
    )
    env = selection_env(env, selection)
    if ui_skipped:
        env[SKIP_UI_ENV] = "1"
    else:
        env.pop(SKIP_UI_ENV, None)

    # T-543: the gate times itself, every run, and writes the result somewhere durable. The
    # start line goes out BEFORE the first suite so that a killed or starved run — the one
    # worth knowing about — still leaves a trace. See `gatelog.py`.
    run_id = gatelog.new_run_id()
    started = time.monotonic()
    # `ops/merge-runner.sh` waits for the box to clear before gating, but that wait is capped
    # (45 min) so a stuck worker or a leaked process cannot hold every merge for ever. When the
    # cap expires it gates anyway and says what it gave up waiting for. Printed AND recorded,
    # because the run is still a real gate - just not a measurement of the code.
    contended = os.environ.get(CONTENDED_ENV, "").strip() or None
    if contended:
        print(f"gate: CONTENDED by {contended}", flush=True)
    gatelog.append(
        gatelog.start_record(
            run_id,
            contended=contended,
            klass=decision.label,
            # A resumed gate ran only the suites after a stopped one: never a whole-gate duration.
            phase=f"resume:{args.resume_after}" if args.resume_after else args.phase,
            source=source.description,
            n_files=len(decision.files),
            crates=list(selection.crates) if selection.crates else None,
            crate_selection=selection.reason,
            branch=current_branch(root),
            sha=head_sha(root),
            root=root,
        )
    )
    print(f"gate: timing   = run {run_id} -> {gatelog.log_path()}", flush=True)

    # Read the timing history ONCE, before the first suite, so a suite's wall-clock baseline is
    # the history as it stood when this gate started rather than a moving target that includes
    # this run's own earlier suites.
    try:
        history_records = gatelog.read()
    except Exception:  # pragma: no cover - gatelog.read already swallows OSError
        history_records = []
    diagnoses: list = []

    result = 0
    for n, cmd in enumerate(commands, 1):
        print(f"gate: running {' '.join(cmd)}", flush=True)
        cmd_started = time.monotonic()
        wall_started = time.time()
        rc = subprocess.run(cmd, cwd=root, env=env, check=False).returncode
        elapsed = time.monotonic() - cmd_started
        gatelog.append(gatelog.suite_record(run_id, cmd=cmd, seconds=elapsed, rc=rc))
        kept_files = keep_junit(root, run_id, n, cmd, since=wall_started)
        for kept in kept_files:
            print(f"gate: junit    = {kept}", flush=True)
        print(f"gate: {' '.join(cmd)} took {elapsed:.0f}s (exit {rc})", flush=True)
        # T-item 4: why this suite cost what it cost — per test, per crate, split by whether
        # THIS diff touched the crate. Printed immediately after the duration it explains, so
        # the two are read together in the log and on the dashboard.
        diagnose_kept(
            kept_files, source=source, cmd=cmd, records=history_records, out=diagnoses
        )
        if rc != 0:
            print(f"gate: FAILED {' '.join(cmd)} (exit {rc})", file=sys.stderr)
            result = rc
            break
    total = time.monotonic() - started

    # One verdict for the whole gate (the worst suite wins), recorded next to the duration it
    # qualifies and alerted ONCE per run. Wrapped for the same reason `diagnose_kept` is.
    verdict = None
    try:
        verdict = gatediag.merge(diagnoses)
        if verdict is not None:
            print(f"gate: verdict  = {verdict.line()}", flush=True)
            gatediag.announce(verdict, root=root, run_id=run_id)
    except Exception as e:  # pragma: no cover - by design
        print(f"gate: timing   = verdict unavailable ({e})", file=sys.stderr)
    gatelog.append(
        gatelog.end_record(
            run_id,
            klass=decision.label,
            phase=args.phase,
            seconds=total,
            rc=result,
            extra=verdict.record_fields() if verdict is not None else None,
        )
    )
    if result != 0:
        return result
    print(f"gate: passed ({decision.label}) in {total:.0f}s", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
