"""ops/roles.py is the one role->session map (incident 2026-09-24 04:07); ops/launch.sh's `case`
creates the sessions and must name the same ones."""
import importlib.util
import os
import re

OPS = os.path.join(os.path.dirname(__file__), "..", "..", "ops")
_spec = importlib.util.spec_from_file_location("roles", os.path.join(OPS, "roles.py"))
R = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(R)


def test_launch_sh_creates_the_sessions_the_map_names():
    text = open(os.path.join(OPS, "launch.sh")).read()
    launched = dict(re.findall(r"^\s*([a-z-]+)\)\s+SESSION=(\w+);", text, re.M))
    assert launched == R.ROLE_SESSION


def test_live_roles_are_the_tmux_ones_the_daemon_can_relaunch():
    assert set(R.LIVE_ROLES) <= set(R.ROLE_SESSION) and "supervisor" not in R.LIVE_ROLES
