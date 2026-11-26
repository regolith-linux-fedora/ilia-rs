"""The Help page (upstream #175): the last tab in every mode, reached with
Alt+h or the arrows, which walk the tabs and wrap at both ends.

The cases see which page is up through Enter: on a list page it prints the
highlighted row, on Help it does nothing and the window stays open.
"""

import json
import select
import subprocess
import time
from pathlib import Path

import pytest

from parity.conftest import BINARY, ROOT
from parity.fakes.executables import install_app_fakes
from parity.fakes.servers import GET_BINDING_STATE, I3Server

ITEMS = (ROOT / "parity/fixtures/textlist-items.txt").read_text()
XCLIP = Path("/usr/bin/xclip")
HELPER = next(
    (
        path
        for path in (BINARY.parent / "ilia-calculator", Path("/usr/libexec/ilia-calculator"))
        if path.is_file()
    ),
    None,
)
GET_CONFIG = 9


def _read_line(app, timeout):
    ready, _, _ = select.select([app.proc.stdout], [], [], timeout)
    return app.proc.stdout.readline() if ready else None


def _enter_does_nothing(app):
    app.key("Return")
    assert _read_line(app, 0.5) is None, "Enter acted on a page"
    assert app.proc.poll() is None, app.finish()


def _clipboard(env):
    result = subprocess.run(
        [str(XCLIP), "-selection", "clipboard", "-o"],
        env=env, text=True, capture_output=True, timeout=5,
    )
    return result.stdout if result.returncode == 0 else None


def _runs(transcript):
    if not transcript.exists():
        return []
    return [
        bytes.fromhex(record["payload_hex"]).decode()
        for record in map(json.loads, transcript.read_text().splitlines())
        if record["message_type"] == 0
    ]


def test_alt_h_opens_help_and_the_arrows_wrap_around_it(isolated_env, run_ilia):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
    app.key("alt+h")
    _enter_does_nothing(app)
    # Right on the last tab lands on the first, Left on the first on the last.
    app.key("Right")
    app.key("Left")
    _enter_does_nothing(app)
    # Ctrl+f opens no search box over the list, which would take the keys.
    app.key("ctrl+f")
    app.key("Left")
    app.key("Return")
    assert app.finish()[:2] == (0, "Alpha\n")


def test_a_page_file_owning_h_leaves_help_without_a_letter(isolated_env, run_ilia):
    env, _ = isolated_env
    pages = Path(env["XDG_CONFIG_HOME"]) / "ilia/pages"
    pages.mkdir(parents=True)
    (pages / "hosts.page").write_text("[Ilia Page]\nName=Hosts\nKey=h\nItems=one;two\n")
    app = run_ilia(env, "-p", "hosts")
    # Alt+h stays with the page; Help is still the second tab.
    app.key("alt+h")
    app.key("Right")
    _enter_does_nothing(app)
    app.key("Right")
    app.key("alt+h")
    app.key("Return")
    assert app.finish()[:2] == (0, "one\n")


@pytest.mark.skipif(not XCLIP.exists(), reason="xclip is not installed")
def test_ctrl_keys_on_help_leave_the_clipboard_alone(isolated_env, run_ilia):
    # The entry is off on Help: Ctrl+c has nothing to copy and must not
    # empty the clipboard, and Ctrl+v and Ctrl+x change nothing.
    env, _ = isolated_env
    owner = subprocess.Popen(
        [str(XCLIP), "-selection", "clipboard", "-i", "-quiet"],
        env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        owner.stdin.write(b"kept")
        owner.stdin.close()
        time.sleep(0.2)
        app = run_ilia(env, "-p", "textlist", stdin=ITEMS)
        app.type("needle")
        app.key("alt+h")
        for key in ("ctrl+a", "ctrl+c", "ctrl+x", "ctrl+v"):
            app.key(key)
        time.sleep(0.3)
        assert _clipboard(env) == "kept"
        _enter_does_nothing(app)
        # The list page keeps its own text.
        app.key("alt+t")
        app.key("Return")
        assert app.finish()[:2] == (0, "Needle Item\n")
    finally:
        owner.kill()
        owner.wait()


def test_i3_bound_keys_of_the_described_page_do_nothing_on_help(isolated_env, run_ilia, tmp_path):
    # The Applications page keeps Ctrl+f, Ctrl+z and Ctrl+= from i3. On Help
    # they stay with ilia too and do nothing; another bound key goes to i3.
    env, bindir = isolated_env
    install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    config = {"included_configs": [{"variable_replaced_contents": (
        "bindsym Control+f exec favourite-key\n"
        "bindsym Control+z exec undo-key\n"
        "bindsym Control+equal exec form-key\n"
        "bindsym Control+q exec unused-key\n"
    )}]}
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    replies = {GET_CONFIG: config, GET_BINDING_STATE: {"name": "default"}}
    with I3Server(socket_path, replies, transcript):
        app = run_ilia(env, "-p", "apps")
        time.sleep(0.5)
        app.key("alt+h")
        for key in ("ctrl+f", "ctrl+z", "ctrl+equal"):
            subprocess.run(["/usr/bin/xdotool", "key", key], env=env, check=True)
        time.sleep(0.3)
        assert app.proc.poll() is None, app.finish()
        assert _runs(transcript) == []
        subprocess.run(["/usr/bin/xdotool", "key", "ctrl+q"], env=env, check=True)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and not _runs(transcript):
            time.sleep(0.02)
        app.proc.kill()
        app.finish()
    assert _runs(transcript) == ["exec unused-key"]


@pytest.mark.skipif(HELPER is None, reason="no ilia-calculator beside the binary or installed")
def test_calculator_arrows_switch_pages_only_from_the_ends_of_the_text(isolated_env, run_ilia):
    env, _ = isolated_env
    for name in list(env):
        if name.startswith("LC_") or name in ("LANG", "LANGUAGE"):
            del env[name]
    env["LC_ALL"] = "C.UTF-8"
    app = run_ilia(env, "-p", "calculator")
    try:
        app.type("12+3")
        # Inside the text the arrows move the cursor.
        for key in ("Left", "Left", "Right", "Right"):
            app.key(key)
        # At the end, Right goes on to Help, and wraps back.
        app.key("Right")
        _enter_does_nothing(app)
        app.key("Right")
        # With text selected, Left only ends the selection; at the start it
        # switches.
        app.key("shift+Home")
        app.key("Left")
        app.key("Left")
        _enter_does_nothing(app)
        app.key("Left")
        app.key("Return")
        assert _read_line(app, 5) == "15\n"
    finally:
        # Without a clipboard manager ilia serves the answer after its
        # window closes.
        app.proc.kill()
        app.finish()
