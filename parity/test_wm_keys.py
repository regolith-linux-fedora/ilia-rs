"""Window-manager and desktop keys while ilia is open (upstream #171).

Under i3 (X11) ilia holds the keyboard, so it asks the window manager for
its binding mode and configuration and has it run the binding a press
matches; the keys ilia uses itself stay with it. Under sway the compositor
runs its bindings and tells subscribers; ilia closes, unless the binding
only switches the keyboard layout. The fakes answer as i3 and sway do; the
transcripts show what ilia asked them to run.
"""

import json
import subprocess
import sys
import time
from pathlib import Path

import pytest

from parity.conftest import ROOT
from parity.fakes.executables import install_app_fakes
from parity.fakes.servers import EVENT_BINDING, GET_BINDING_STATE, I3Server

ITEMS = (ROOT / "parity/fixtures/textlist-items.txt").read_text()
GET_CONFIG = 9


def _i3_config(*files):
    return {"included_configs": [{"variable_replaced_contents": text} for text in files]}


def _records(transcript):
    return [json.loads(line) for line in transcript.read_text().splitlines()]


def _runs(transcript):
    return [
        bytes.fromhex(record["payload_hex"]).decode()
        for record in _records(transcript)
        if record["message_type"] == 0
    ]


def _xdotool(app, *args):
    subprocess.run(["/usr/bin/xdotool", *args], env=app.env, check=True)


def _wait_exit(app, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if app.proc.poll() is not None:
            return True
        time.sleep(0.02)
    return False


def _i3_session(env, tmp_path, config, state="default"):
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    replies = {GET_CONFIG: config, GET_BINDING_STATE: {"name": state}}
    return I3Server(socket_path, replies, transcript), transcript


I3_BINDINGS = (
    'set $unused nothing\n'
    'bindsym Control+Mod1+t [class="Fixture  App"] exec notify-send "a  b"; nop\n'
    'bindsym t exec typed-key\n'
    'bindsym Control+a exec select-all\n'
    'bindsym Down exec down-key\n'
    'bindsym Mod1+plus exec grow\n'
    'bar {\n'
    '    bindsym Control+Mod1+b exec bar-binding\n'
    '}\n'
)


def test_i3_binding_runs_once_as_written_and_ilia_closes(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    server, transcript = _i3_session(env, tmp_path, _i3_config(I3_BINDINGS))
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        start = time.monotonic()
        _xdotool(app, "key", "ctrl+alt+t")
        assert _wait_exit(app, 2), "ilia stayed open"
        assert time.monotonic() - start < 2
        code, stdout, _ = app.finish()
    assert code == 0
    assert stdout == ""
    # The command exactly as the configuration holds it: criteria, quoting
    # and the chain unchanged, once.
    assert _runs(transcript) == ['[class="Fixture  App"] exec notify-send "a  b"; nop']


def test_i3_keys_ilia_uses_stay_with_it_even_when_bound(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    server, transcript = _i3_session(env, tmp_path, _i3_config(I3_BINDINGS))
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        # A key bound only inside a bar block is no binding of i3's.
        _xdotool(app, "key", "ctrl+alt+b")
        for key in ("Down", "Up", "ctrl+a", "alt+plus", "alt+minus", "BackSpace"):
            _xdotool(app, "key", key)
        _xdotool(app, "type", "--delay", "5", "zq t")
        time.sleep(0.3)
        assert app.proc.poll() is None, app.finish()
        app.key("Return")
        code, stdout, _ = app.finish()
    assert code == 0
    assert stdout == "zq t\n"
    assert _runs(transcript) == []


def test_i3_keys_a_page_uses_stay_with_it(isolated_env, run_ilia, tmp_path):
    # On the Applications page Ctrl+Shift+C copies the program path, Ctrl+=
    # opens the add-app form, Ctrl+z brings back hidden apps, and in the
    # form Tab moves between its fields. i3 binding them does not take them.
    env, bindir = isolated_env
    install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    config = _i3_config(
        "bindsym Control+Shift+c exec copy-key\n"
        "bindsym Control+z exec undo-key\n"
        "bindsym Control+equal exec form-key\n"
        "bindsym Tab exec tab-key\n"
        "bindsym Shift+Tab exec shift-tab-key\n"
        "bindsym Control+q exec unused-key\n"
    )
    server, transcript = _i3_session(env, tmp_path, config)
    with server:
        app = run_ilia(env, "-p", "apps")
        for key in ("ctrl+shift+c", "ctrl+z", "ctrl+equal", "Tab", "shift+Tab", "Tab"):
            _xdotool(app, "key", key)
        time.sleep(0.3)
        assert app.proc.poll() is None, app.finish()
        assert _runs(transcript) == []
        # Outside the form a key the page does not use goes to i3. ilia
        # closes its window but runs on, serving the path Ctrl+Shift+C put
        # on the clipboard, as there is no clipboard manager here.
        app.key("Escape")
        time.sleep(0.2)
        assert app.proc.poll() is None, "Escape closed the form and ilia"
        _xdotool(app, "key", "ctrl+q")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and not _runs(transcript):
            time.sleep(0.02)
        app.proc.kill()
        app.finish()
    assert _runs(transcript) == ["exec unused-key"]


@pytest.mark.parametrize("state,expected", [
    ("default", "exec hex-name"),
    ("fixture mode", "exec in-mode"),
])
def test_i3_key_names_and_the_current_mode(isolated_env, run_ilia, tmp_path, state, expected):
    # i3 reads key names with libxkbcommon: a hexadecimal name is a key.
    # Only the bindings of the mode i3 is in count.
    env, _ = isolated_env
    config = _i3_config(
        "bindsym Control+Mod1+0x79 exec hex-name\n"
        'mode "fixture mode" {\n'
        "    bindsym Control+Mod1+y exec in-mode\n"
        "}\n"
    )
    server, transcript = _i3_session(env, tmp_path, config, state=state)
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        _xdotool(app, "key", "ctrl+alt+y")
        assert _wait_exit(app, 2), "ilia stayed open"
        app.finish()
    assert _runs(transcript) == [expected]


def test_i3_release_binding_runs_when_the_key_is_released(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    config = _i3_config("bindsym --release Control+Mod1+y exec on-release\n")
    server, transcript = _i3_session(env, tmp_path, config)
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        _xdotool(app, "keydown", "ctrl+alt+y")
        time.sleep(0.5)
        assert app.proc.poll() is None, "ilia closed on the press"
        assert _runs(transcript) == []
        _xdotool(app, "keyup", "ctrl+alt+y")
        assert _wait_exit(app, 2), "ilia stayed open"
        app.finish()
    assert _runs(transcript) == ["exec on-release"]


@pytest.mark.parametrize("wm", ["i3", "sway"])
def test_unusable_socket_is_reported_once_and_ilia_works_on(isolated_env, run_ilia, tmp_path, wm):
    env, _ = isolated_env
    missing = tmp_path / f"missing-{wm}.sock"
    env["I3SOCK" if wm == "i3" else "SWAYSOCK"] = str(missing)
    app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
    for key in ("F9", "ctrl+alt+t", "F10"):
        _xdotool(app, "key", key)
    app.type("needle")
    time.sleep(0.3)
    assert app.proc.poll() is None, app.finish()
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert (code, stdout) == (0, "Needle Item\n")
    assert stderr.count(str(missing)) == 1, stderr


def test_desktop_key_reaches_the_program_that_grabs_it(isolated_env, run_ilia, tmp_path):
    # No window manager socket at all: the volume key, pressed with Control
    # held, goes to the program grabbing it once ilia has let go.
    env, _ = isolated_env
    log = tmp_path / "grabbed.log"
    log.write_text("")
    grabber = subprocess.Popen(
        [sys.executable, str(ROOT / "parity/fakes/key_grabber.py"), str(log), "XF86AudioLowerVolume"],
        env=env, stdout=subprocess.PIPE, text=True,
    )
    try:
        assert grabber.stdout.readline().strip() == "ready"
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        _xdotool(app, "keydown", "ctrl")
        _xdotool(app, "key", "XF86AudioLowerVolume")
        _xdotool(app, "keyup", "ctrl")
        assert _wait_exit(app, 2), "ilia stayed open"
        app.finish()
        time.sleep(0.3)
        presses = [line for line in log.read_text().splitlines() if line.startswith("press")]
    finally:
        grabber.kill()
        grabber.wait()
    # Control is the X modifier bit 2.
    assert presses == ["press XF86AudioLowerVolume 4"]


def _sway_session(env, tmp_path):
    socket_path = tmp_path / "sway.sock"
    transcript = tmp_path / "sway-ipc.jsonl"
    env["SWAYSOCK"] = str(socket_path)
    return I3Server(socket_path, {}, transcript), transcript


def _binding_event(command):
    return {
        "change": "run",
        "binding": {
            "command": command, "event_state_mask": ["Mod4"], "input_code": 0,
            "symbol": "x", "input_type": "keyboard",
        },
    }


def test_sway_binding_closes_ilia_without_running_it_again(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    server, transcript = _sway_session(env, tmp_path)
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        assert server.subscribed.wait(5), "ilia did not subscribe to binding events"
        assert server.send_event(EVENT_BINDING, _binding_event("exec fixture-sway")) == 1
        assert _wait_exit(app, 2), "ilia stayed open"
        code, stdout, _ = app.finish()
    assert (code, stdout) == (0, "")
    assert _runs(transcript) == []
    subscriptions = [
        bytes.fromhex(record["payload_hex"]).decode()
        for record in _records(transcript) if record["message_type"] == 2
    ]
    assert [json.loads(payload) for payload in subscriptions] == [["binding"]]


def test_sway_layout_switch_binding_keeps_ilia_open(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    server, transcript = _sway_session(env, tmp_path)
    with server:
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        assert server.subscribed.wait(5), "ilia did not subscribe to binding events"
        server.send_event(EVENT_BINDING, _binding_event(
            'input type:keyboard xkb_switch_layout next; input "1:1:Fixture Keyboard" xkb_switch_layout 0'
        ))
        time.sleep(0.5)
        assert app.proc.poll() is None, app.finish()
        app.type("needle")
        time.sleep(0.2)
        assert app.proc.poll() is None, app.finish()
        # A chain that does more than switch the layout closes ilia.
        server.send_event(EVENT_BINDING, _binding_event("input * xkb_switch_layout next, exec fixture"))
        assert _wait_exit(app, 2), "ilia stayed open"
        code, stdout, _ = app.finish()
    assert (code, stdout) == (0, "")
    assert _runs(transcript) == []
