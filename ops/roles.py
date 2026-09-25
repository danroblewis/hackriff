"""Which tmux session each top-level role runs in - the one map (incident 2026-09-24 04:07).

ops/launch.sh creates the sessions (its `case` must agree; py/tests/test_roles.py checks it);
ops/watchdog.py checks the LIVE_ROLES are alive, ops/alert.py relays alarms into them and names a
missing one, ops/worklog.py names a session's owner. The shell and work-runner relays to the
coordinator still name `dev` literally. The supervisor runs in the user's own terminal,
not in tmux, so the daemon cannot see or relaunch it - it is in the map, not in LIVE_ROLES.
The explorer (T-923) is a bounded window that ends by design, so it is never in LIVE_ROLES either:
relaunching it would take the radio lock again behind the user's back.
"""
ROLE_SESSION = {"coordinator": "dev", "pipeline-manager": "flow", "supervisor": "super", "explorer": "explore"}
SESSION_ROLE = {s: r for r, s in ROLE_SESSION.items()}
LIVE_ROLES = ("coordinator", "pipeline-manager")
