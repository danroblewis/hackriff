"""Two estimates for the 2-hourly digest (user, 2026-09-24): when the merge queue clears, and when
the top Leverage ticket lands - e.g. "queue clears ~01:40; T-801 lands ~02:30".

Both are ESTIMATES from measured medians, and say so ("~"):

  * **queue clears** = now + what is left of the running gate (its start + the full-gate median,
    never negative) + one full gate per BULK_MAX queued branches (a batch carries up to BULK_MAX).
  * **ticket lands**, by where the ticket is:
      - its branch is queued at position p        -> the gate of batch ceil(p / BULK_MAX) ends
      - a work-runner worker is running on it     -> max(now, started + worker median) + review
                                                     median, then the queue ahead of it + a gate
      - review-failed / gate-failed / blocked / waiting on a person, or no claim at all
                                                  -> no estimate, and the reason instead
Flakes, reds and rewinds are not modelled: a red gate pushes everything by about one gate.
"""

from __future__ import annotations

import math
import os
from datetime import datetime, timedelta

BULK_MAX = int(os.environ.get("BULK_MAX") or 15)
NO_ESTIMATE = ("review-failed", "gate-failed", "blocked", "no-work", "error", "timeout", "conflict", "uncommitted")


def _hm(t: datetime) -> str:
    return t.strftime("%H:%M")


def queue_clears(now: datetime, depth: int, gate_started: datetime | None, gate_min: float,
                 bulk_max: int = BULK_MAX) -> datetime | None:
    """None when there is nothing queued and nothing gating."""
    left = max(0.0, gate_min - (now - gate_started).total_seconds() / 60) if gate_started else 0.0
    if depth <= 0 and gate_started is None:
        return None
    return now + timedelta(minutes=left + math.ceil(max(depth, 0) / bulk_max) * gate_min)


def ticket_lands(now: datetime, ticket: str, branch: str, queue: list[str], claim: dict | None,
                 gate_started: datetime | None, gate_min: float, work_min: float, review_min: float,
                 board_status: str | None = None, bulk_max: int = BULK_MAX) -> tuple[datetime | None, str]:
    """(estimate or None, why)."""
    left = max(0.0, gate_min - (now - gate_started).total_seconds() / 60) if gate_started else 0.0
    if board_status in ("done", "cancelled"):
        return None, f"already {board_status}"
    if branch in queue:
        k = math.ceil((queue.index(branch) + 1) / bulk_max)
        return now + timedelta(minutes=left + k * gate_min), f"queued, position {queue.index(branch) + 1}"
    state = (claim or {}).get("state")
    if state == "running" and (claim or {}).get("kind") in ("work", "fix"):
        started = datetime.fromtimestamp(float(claim.get("started") or now.timestamp()))
        done = max(now, started + timedelta(minutes=work_min)) + timedelta(minutes=review_min)
        # it joins the queue then; whatever is queued now has cleared or is in the batch with it
        return done + timedelta(minutes=max(left - (done - now).total_seconds() / 60, 0) + gate_min), "a worker is on it"
    if state in NO_ESTIMATE:
        return None, f"{state} - needs the coordinator"
    if board_status == "blocked":
        return None, "blocked on the board"
    return None, "nobody is working on it" if not claim else f"claim {state}"


def digest_line(now: datetime, q_eta: datetime | None, depth: int, ticket: str | None,
                t_eta: datetime | None, t_why: str) -> str:
    q = f"queue clears ~{_hm(q_eta)} ({depth} queued)" if q_eta else "queue empty, nothing gating"
    if not ticket:
        return f"ETA: {q}"
    t = f"{ticket} lands ~{_hm(t_eta)} ({t_why})" if t_eta else f"{ticket}: no estimate - {t_why}"
    return f"ETA: {q}; {t} - medians, flakes and reds not modelled"
