"""Favourite applications and Ctrl+Enter on the Applications page (upstream
#180, ported ahead of upstream).

The settings use the memory backend, so the cases read the pins through the
order of the rows: Enter launches the first row once the filter is empty,
and Down moves to the row below. Ctrl+Enter is checked against the fake
window manager's transcript: the workspace list asked for, the switch
command, and the launch only after the switch was answered.
"""

import json
import subprocess
import time
from pathlib import Path

import pytest

from parity.conftest import ROOT
from parity.fakes.executables import install_app_fakes
from parity.fakes.servers import GET_WORKSPACES, RUN_COMMAND, I3Server

# The page's rows with an empty filter and no pins: no launches yet, so by
# name. The fixture directory adds Alpha and Terminal Fixture.
EXTRA_APPS = ("aardvark", "mango", "zebra")

WORKSPACES = [
    {"num": 1, "name": "1", "focused": True},
    {"num": 2, "name": "2:web", "focused": False},
    {"num": -1, "name": "music", "focused": False},
]


def _install_apps(env, bindir, tmp_path):
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    applications = Path(env["XDG_DATA_HOME"]) / "applications"
    for name in EXTRA_APPS:
        (applications / f"{name}.desktop").write_text(
            "[Desktop Entry]\nType=Application\n"
            f"Name={name.capitalize()} Fixture\nExec=fixture-app {name}\n"
        )
    return log


def _start(run_ilia, env, *args):
    app = run_ilia(env, "-p", "apps", *args)
    # The rows arrive just after the window (upstream #162); Ctrl+f and
    # Alt+Up act on the highlighted row, so let the list come first.
    time.sleep(0.5)
    return app


def _keys(app, *keys):
    for key in keys:
        app.key(key)


def _launched(log, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        lines = [
            line for line in (log.read_text() if log.exists() else "").splitlines()
            if line.startswith("app:")
        ]
        if lines:
            return lines
        time.sleep(0.02)
    return []


def _clear(app, text):
    _keys(app, *["BackSpace"] * len(text))


def test_ctrl_f_pins_ahead_of_every_other_app(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    app = _start(run_ilia, env)
    app.type("zebra")
    app.key("ctrl+f")
    _clear(app, "zebra")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:zebra"]


def test_ctrl_f_leaves_the_filter_text_alone(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    app = _start(run_ilia, env)
    app.type("zebr")
    app.key("ctrl+f")
    # Had Ctrl+f typed an f, "zebrfa" would match nothing.
    app.type("a")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:zebra"]


def test_ctrl_f_again_unpins(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    app = _start(run_ilia, env)
    app.type("zebra")
    _keys(app, "ctrl+f", "ctrl+f")
    _clear(app, "zebra")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:aardvark"]


def test_alt_up_moves_a_pin_ahead_of_the_one_above(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    app = _start(run_ilia, env)
    for name in ("zebra", "mango"):
        app.type(name)
        app.key("ctrl+f")
        _clear(app, name)
    # Rows: Zebra, Mango (pinned), then the rest. Down highlights Mango,
    # Alt+Up puts it first and keeps it highlighted, so Down reaches Zebra.
    # Without the move, Down would reach Aardvark.
    _keys(app, "Down", "alt+Up", "Down", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:zebra"]


def test_alt_down_does_nothing_while_the_filter_has_words(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    app = _start(run_ilia, env)
    for name in ("zebra", "mango"):
        app.type(name)
        app.key("ctrl+f")
        _clear(app, name)
    # "fixture" lists every app, the pins first. Alt+Down on Zebra stores
    # nothing, so Zebra still leads once the filter is empty.
    app.type("fixture")
    _keys(app, "alt+Down")
    _clear(app, "fixture")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:zebra"]


def _wm_session(env, tmp_path, wm, log):
    socket_path = tmp_path / f"{wm}.sock"
    transcript = tmp_path / f"{wm}-ipc.jsonl"
    env["I3SOCK" if wm == "i3" else "SWAYSOCK"] = str(socket_path)
    server = SwitchRecorder(socket_path, {GET_WORKSPACES: WORKSPACES}, transcript)
    server.launch_log = log
    return server, transcript


class SwitchRecorder(I3Server):
    """Notes, for each command request, whether the app had been started
    before the window manager answered it."""

    launch_log = None

    def record(self, request, transcript_line):
        message_type, _ = request
        if message_type == RUN_COMMAND:
            entry = json.loads(transcript_line)
            entry["launched_before"] = bool(_launched(self.launch_log, timeout=0))
            transcript_line = json.dumps(entry, sort_keys=True) + "\n"
        super().record(request, transcript_line)


def _requests(transcript):
    """The window manager requests that matter here: workspace lists and
    commands, in order (key-binding queries left out)."""
    requests = []
    for line in transcript.read_text().splitlines():
        record = json.loads(line)
        payload = bytes.fromhex(record["payload_hex"]).decode()
        if record["message_type"] == GET_WORKSPACES:
            requests.append(("workspaces", payload))
        elif record["message_type"] == RUN_COMMAND:
            requests.append(("command", payload, record["launched_before"]))
    return requests


@pytest.mark.parametrize(
    "wm, key, switch",
    [
        ("i3", "ctrl+Return", 'workspace number "3"'),
        ("i3", "ctrl+KP_Enter", 'workspace number "3"'),
        ("sway", "ctrl+Return", "workspace number '3'"),
    ],
)
def test_ctrl_enter_switches_to_the_next_free_workspace_then_launches(
    isolated_env, run_ilia, tmp_path, wm, key, switch
):
    # 1 and 2:web take 1 and 2; "music" has no number.
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    server, transcript = _wm_session(env, tmp_path, wm, log)
    with server:
        app = _start(run_ilia, env)
        app.type("mango")
        app.key(key)
        code, stdout, _ = app.finish(timeout=8)
    assert code == 0
    assert stdout == ""
    assert _launched(log) == ["app:mango"]
    assert _requests(transcript) == [("workspaces", ""), ("command", switch, False)]


def test_ctrl_enter_names_the_workspace_from_its_x_resource(isolated_env, run_ilia, tmp_path):
    xrdb = Path("/usr/bin/xrdb")
    if not xrdb.exists():
        pytest.skip("xrdb is not installed")
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    resources = tmp_path / "resources"
    # As i3 reads it: \\ is one backslash, a backslash before other
    # characters stays.
    resources.write_text('wm.workspace.03.name: 3: Notes "draft" C:\\\\x \\w\n')
    subprocess.run([str(xrdb), "-nocpp", "-load", str(resources)], env=env, check=True)
    try:
        server, transcript = _wm_session(env, tmp_path, "i3", log)
        with server:
            app = _start(run_ilia, env)
            app.type("mango")
            app.key("ctrl+Return")
            code, _, _ = app.finish(timeout=8)
    finally:
        subprocess.run([str(xrdb), "-remove"], env=env, check=True)
    assert code == 0
    assert _launched(log) == ["app:mango"]
    assert _requests(transcript) == [
        ("workspaces", ""),
        ("command", 'workspace number "3: Notes \\"draft\\" C:\\\\x \\\\w"', False),
    ]


def test_ctrl_enter_does_nothing_without_a_row_or_from_help(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install_apps(env, bindir, tmp_path)
    server, transcript = _wm_session(env, tmp_path, "i3", log)
    with server:
        app = _start(run_ilia, env)
        app.type("qqq")
        app.key("ctrl+Return")
        _clear(app, "qqq")
        app.type("mango")
        # The Help tab: the filter is off there, as for Enter.
        _keys(app, "Right", "ctrl+Return")
        time.sleep(0.5)
        assert app.proc.poll() is None, app.finish()
        assert _launched(log, timeout=0) == []
        assert _requests(transcript) == []
        # Back on the Applications tab the same key launches.
        _keys(app, "Left", "ctrl+Return")
        code, _, _ = app.finish(timeout=8)
    assert code == 0
    assert _launched(log) == ["app:mango"]
    assert _requests(transcript) == [("workspaces", ""), ("command", 'workspace number "3"', False)]
