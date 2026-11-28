"""Desktop-entry actions on the Applications page (upstream #179, which
replaces #173; ported ahead of upstream).

The cases see the rows through what Enter starts: the fake runner logs the
scope it is asked for, and the fake program its arguments. Right shows an
app's actions beneath it and, once they are shown, highlights the first;
Left closes them; typed text narrows them while they are open; a colon in
the filter lists actions only.
"""

import json
import re
import subprocess
import sys
import time
from pathlib import Path

from parity.conftest import ROOT
from parity.fakes.executables import install_app_fakes
from parity.fakes.servers import GET_WORKSPACES, RUN_COMMAND, I3Server

QUOKKA = (
    "[Desktop Entry]\nType=Application\nName=Quokka Browser\n"
    "Exec=fixture-app quokka\n"
    "Actions=new-window;private-window;;ghost;profiles;private-window;\n"
    "[Desktop Action new-window]\nName=New Window\nExec=fixture-app quokka --new-window\n"
    "[Desktop Action private-window]\nName=Private Window\n"
    "Exec=fixture-app quokka --private\n"
    "[Desktop Action profiles]\nName=Profile Manager\nExec=fixture-app quokka --profiles\n"
    "[Desktop Action unlisted]\nName=Unlisted\nExec=fixture-app quokka --unlisted\n"
)


def _install(env, bindir, tmp_path):
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    applications = Path(env["XDG_DATA_HOME"]) / "applications"
    (applications / "quokka.desktop").write_text(QUOKKA)
    return log


def _start(run_ilia, env):
    app = run_ilia(env, "-p", "apps")
    # The rows arrive just after the window (upstream #162); Right acts on
    # the highlighted row, so let the list come first.
    time.sleep(0.5)
    return app


def _keys(app, *keys):
    for key in keys:
        app.key(key)


def _log_lines(log, timeout=5):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        text = log.read_text() if log.exists() else ""
        if any(line.startswith("app:") for line in text.splitlines()):
            return text.splitlines()
        time.sleep(0.02)
    return []


def _launched(log, timeout=5):
    return [line for line in _log_lines(log, timeout) if line.startswith("app:")]


def _run(run_ilia, env, text, *keys):
    app = _start(run_ilia, env)
    if text:
        app.type(text)
    _keys(app, *keys)
    return app


def test_a_word_of_the_action_lists_it_and_enter_starts_it_in_a_scope(
    isolated_env, run_ilia, tmp_path
):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quokka private", "Return")
    assert app.finish(timeout=8)[0] == 0
    lines = _log_lines(log)
    assert len(lines) == 2, lines
    assert re.fullmatch(
        r"systemd:--user --scope --unit "
        r"run_ilia_quokka\.desktop_[0-9a-f]{8}\.scope fixture-app quokka --private",
        lines[0],
    ), lines
    assert lines[1] == "app:quokka --private"


def test_the_app_name_alone_lists_no_action(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    # "quokka" lists the app alone: Down finds no second row, and Enter
    # starts the app itself.
    app = _run(run_ilia, env, "quokka", "Down", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka"]


def test_right_shows_the_actions_and_right_again_highlights_the_first(
    isolated_env, run_ilia, tmp_path
):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quokka", "Right", "Right", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --new-window"]


def test_actions_beneath_their_app_keep_the_order_of_the_actions_key(
    isolated_env, run_ilia, tmp_path
):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    # New Window, Private Window, Profile Manager: the repeated, empty and
    # unknown identifiers and the unlisted group give no row.
    app = _run(run_ilia, env, "quokka", "Right", "Down", "Down", "Down", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --profiles"]


def test_left_closes_the_actions_and_highlights_the_app(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quokka", "Right", "Right", "Left", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka"]


def test_alt_enter_shows_the_actions_as_right_does(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quokka", "alt+Return", "alt+Return", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --new-window"]


def test_alt_enter_on_an_app_without_actions_starts_nothing(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "needle", "alt+Return")
    time.sleep(0.5)
    assert app.proc.poll() is None, app.finish()
    app.key("Escape")
    assert app.finish()[0] == 0
    assert _launched(log, timeout=0) == []


def test_typing_narrows_the_open_actions(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    # "prof" narrows Quokka's actions to Profile Manager and leaves the
    # filter text alone; Right then highlights the one left.
    app = _run(run_ilia, env, "quokka", "Right")
    app.type("prof")
    _keys(app, "Right", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --profiles"]


def test_backspace_widens_the_narrowed_actions_again(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quokka", "Right")
    app.type("prof")
    _keys(app, *["BackSpace"] * 4, "Right", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --new-window"]


def test_a_colon_lists_the_actions_of_the_apps_before_it(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    app = _run(run_ilia, env, "quo:priv", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --private"]


def test_ctrl_f_on_an_action_pins_nothing(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    # Ctrl+f on New Window leaves the favorites alone: once the filter is
    # empty Alpha Fixture still comes first, not Quokka Browser.
    app = _run(run_ilia, env, "quokka", "Right", "Right", "ctrl+f", "Left")
    _keys(app, *["BackSpace"] * len("quokka"), "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:alpha"]


def test_pinning_an_open_app_keeps_its_actions_shown(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    # Rows: Alpha Fixture, Quokka Browser, Terminal Fixture. Ctrl+f on the
    # open Quokka Browser moves it first; its actions stay beneath it, so
    # Right still highlights the first.
    app = _run(run_ilia, env, "", "Down", "Right", "ctrl+f", "Right", "Return")
    assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --new-window"]


class _SwitchRecorder(I3Server):
    """Notes, for each command request, whether something had been started
    before the window manager answered it."""

    launch_log = None

    def record(self, request, transcript_line):
        message_type, _ = request
        if message_type == RUN_COMMAND:
            entry = json.loads(transcript_line)
            entry["launched_before"] = bool(_launched(self.launch_log, timeout=0))
            transcript_line = json.dumps(entry, sort_keys=True) + "\n"
        super().record(request, transcript_line)


def test_ctrl_enter_starts_an_action_on_the_next_free_workspace(
    isolated_env, run_ilia, tmp_path
):
    env, bindir = isolated_env
    log = _install(env, bindir, tmp_path)
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    workspaces = [{"num": 1, "name": "1", "focused": True}]
    server = _SwitchRecorder(socket_path, {GET_WORKSPACES: workspaces}, transcript)
    server.launch_log = log
    with server:
        app = _run(run_ilia, env, "quokka private", "ctrl+Return")
        assert app.finish(timeout=8)[0] == 0
    assert _launched(log) == ["app:quokka --private"]
    commands = []
    for line in transcript.read_text().splitlines():
        record = json.loads(line)
        if record["message_type"] == RUN_COMMAND:
            payload = bytes.fromhex(record["payload_hex"]).decode()
            commands.append((payload, record["launched_before"]))
    assert commands == [('workspace number "2"', False)]


def test_an_action_of_a_bus_activated_app_is_activated_over_the_bus(
    isolated_env, run_ilia, tmp_path
):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    name = "org.example.Wombat-Mail"
    (Path(env["XDG_DATA_HOME"]) / "applications" / f"{name}.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Wombat Mail\nDBusActivatable=true\n"
        "Exec=fixture-app wombat\nActions=compose;\n"
        "[Desktop Action compose]\nName=Compose Message\nExec=fixture-app wombat --compose\n"
    )
    calls = tmp_path / "bus-app.log"
    service = subprocess.Popen(
        [sys.executable, "-m", "parity.fakes.bus_app", name, str(calls)],
        cwd=ROOT, env={**env, "PATH": "/usr/bin:/bin"},
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        assert service.stdout.readline().strip() == "ready", service.stderr.read()
        app = _run(run_ilia, env, "wombat:compose", "Return")
        assert app.finish(timeout=8)[0] == 0
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and not calls.exists():
            time.sleep(0.02)
        assert calls.read_text().splitlines() == [
            "ActivateAction /org/example/Wombat_Mail compose"
        ]
    finally:
        service.terminate()
        service.wait(timeout=5)
    # Neither Exec line runs.
    assert _launched(log, timeout=0) == []
