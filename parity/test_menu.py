"""Menu pages (upstream #187): page files whose `Entries` list a tree of
submenus, window-manager bindings and commands.

Each case runs under a fake i3, a fake sway and, where it matters, no
window manager. The cases see the rows through what Enter does: a binding
row reaches the window manager as one RUN_COMMAND request, a command row
runs a stand-in that logs its arguments, and a hidden row leaves Enter
with nothing to act on.
"""

import json
import os
import re
import signal
import subprocess
import time
from pathlib import Path

import pytest

from parity.conftest import BINARY
from parity.fakes.executables import _write_executable
from parity.fakes.servers import I3Server

# A description that no binding line follows (`Rest Now`) has no command
# and never matches a binding row.
WM_CONFIG = (
    "## Capture // Grab a region // <Super><Shift>Print ##\n"
    "bindsym $mod+Shift+Print exec fixture-grab --region\n"
    "## Session // Rest Now // <Super>r ##\n"
)

PAGE = """[Ilia Page]
Name=Actions
Icon=system-run
Key=i
Entries=capture;system;sway-only

[Entry capture]
Name=Capture
Entries=region;rest

[Entry region]
Name=Grab a region
Binding=  capture //  GRAB A REGION

[Entry rest]
Name=Rest
Binding=Rest Now

[Entry system]
Name=System
Entries=suspend;never;lights

[Entry suspend]
Name=Suspend
Exec=fixture-run suspend "100%%"

[Entry never]
Name=Never shown
Exec=fixture-run never
When=fixture-fail

[Entry lights]
Name=Lights
Exec=fixture-run lights
When=fixture-pass
State=fixture-state

[Entry sway-only]
Name=Sway tools
OnlyShowIn=sway
Entries=swaytool

[Entry swaytool]
Name=Sway tool
Exec=fixture-run swaytool
"""

SCOPE = re.compile(r"^systemd:--user --scope --unit[= ]run_ilia_actions_[0-9a-f]{8}\.scope -- (.*)$")
# An empty tree for the Windows page under -a.
TREE = {"id": 1, "type": "root", "nodes": [], "floating_nodes": []}


def _sway_version(config_path):
    return {
        "human_readable": "1.11", "loaded_config_file_name": str(config_path),
        "major": 1, "minor": 11, "patch": 0,
    }


@pytest.fixture(params=["i3", "sway"])
def wm(request, isolated_env, tmp_path):
    """A fake i3 or sway holding WM_CONFIG, the page file, and stand-ins
    for the commands its rows run."""
    yield from _session(request.param, isolated_env, tmp_path)


@pytest.fixture(params=["i3", "sway", "other"])
def any_wm(request, isolated_env, tmp_path):
    yield from _session(request.param, isolated_env, tmp_path)


@pytest.fixture
def no_wm(isolated_env, tmp_path):
    yield from _session("other", isolated_env, tmp_path)


class Session:
    def __init__(self, name, env, tmp_path):
        self.name = name
        self.env = env
        self.log = tmp_path / "commands.log"
        self.transcript = tmp_path / "wm-ipc.jsonl"
        self.pids = tmp_path / "state.pids"
        self.page = Path(env["XDG_CONFIG_HOME"]) / "ilia/pages/actions.page"

    def lines(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def runs(self):
        """The commands sent to the window manager."""
        if not self.transcript.exists():
            return []
        return [
            bytes.fromhex(record["payload_hex"]).decode()
            for record in map(json.loads, self.transcript.read_text().splitlines())
            if record["message_type"] == 0
        ]


def _session(name, isolated_env, tmp_path):
    env, bindir = isolated_env
    session = Session(name, env, tmp_path)
    session.page.parent.mkdir(parents=True)
    session.page.write_text(PAGE)
    log = session.log
    # Logs its arguments, then runs the command after `--`.
    _write_executable(
        bindir / "systemd-run",
        f'printf "systemd:%s\\n" "$*" >> "{log}"\n'
        'while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do shift; done\n'
        'shift\nexec "$@"\n',
    )
    _write_executable(bindir / "fixture-run", f'printf "run:%s\\n" "$*" >> "{log}"\n')
    _write_executable(bindir / "fixture-fail", f'printf "when:fail\\n" >> "{log}"\nexit 1\n')
    _write_executable(bindir / "fixture-pass", f'printf "when:pass\\n" >> "{log}"\n')
    # Prints nothing, so that its row's column stays empty, and outlives
    # the 2 s a command may run.
    _write_executable(
        bindir / "fixture-state",
        f'echo "$$" >> "{session.pids}"\n/bin/sleep 30\n',
    )
    if name == "other":
        yield session
        return
    socket_path = tmp_path / f"{name}.sock"
    if name == "i3":
        env["I3SOCK"] = str(socket_path)
        replies = {9: {"included_configs": [{"variable_replaced_contents": WM_CONFIG}]}, 4: TREE}
    else:
        env["SWAYSOCK"] = str(socket_path)
        replies = {9: {"config": WM_CONFIG}, 7: _sway_version(tmp_path / "sway-config"), 4: TREE}
    with I3Server(socket_path, replies, session.transcript):
        yield session


def _wait(predicate, timeout, message):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {message}")


def _stays_open(app, wait=0.5):
    time.sleep(wait)
    assert app.proc.poll() is None, app.finish()


def _alive(pid):
    """Whether the process `pid` exists and has not exited (a zombie that
    nobody collects counts as gone)."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


def _run_without_window(env, *args):
    result = subprocess.run(
        [str(BINARY), *args], env=env, text=True, capture_output=True, timeout=5,
    )
    return result.returncode, result.stdout, result.stderr


def test_a_binding_row_found_from_the_top_runs_the_window_managers_command(wm, run_ilia):
    app = run_ilia(wm.env, "-p", "actions")
    # Deeper rows are searched by their path from the current level.
    app.type("capture region")
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert (code, stdout, stderr) == (0, "", "")
    assert wm.runs() == ["exec fixture-grab --region"]
    assert not any(line.startswith(("run:", "systemd:")) for line in wm.lines())


def test_binding_rows_without_a_command_or_a_window_manager_are_hidden(any_wm, run_ilia):
    app = run_ilia(any_wm.env, "-p", "actions")
    # `Rest Now` has no binding line; with no window manager no binding
    # matches, and Capture, holding only binding rows, goes too.
    for text in ("rest", "capture") if any_wm.name == "other" else ("rest",):
        app.type(text)
        app.key("Return")
        _stays_open(app)
        for _ in text:
            app.key("BackSpace")
    app.key("Escape")
    assert app.finish()[0] == 0
    assert any_wm.runs() == []


def test_an_exec_row_runs_its_words_in_a_scope(any_wm, run_ilia):
    app = run_ilia(any_wm.env, "-p", "actions")
    app.type("suspend")
    app.key("Return")
    code, stdout, _ = app.finish()
    assert (code, stdout) == (0, "")
    scoped = [match.group(1) for line in any_wm.lines() if (match := SCOPE.match(line))]
    assert scoped == ['fixture-run suspend 100%']
    assert "run:suspend 100%" in any_wm.lines()


def test_backspace_and_alt_left_return_to_the_submenu_that_led_in(wm, run_ilia):
    app = run_ilia(wm.env, "-p", "actions")
    # At the top BackSpace does nothing; Capture is highlighted.
    app.key("BackSpace")
    app.key("Down")
    app.key("Return")  # opens System
    app.key("BackSpace")  # back at the top, System highlighted
    app.key("Return")  # opens System again
    app.key("alt+Left")  # and back once more
    app.key("Return")  # System
    # While the filter holds text BackSpace edits it.
    app.type("suspendd")
    app.key("BackSpace")
    app.key("Return")
    assert app.finish()[0] == 0
    assert "run:suspend 100%" in wm.lines()
    assert wm.runs() == []


def test_when_hides_rows_until_it_passes_and_each_runs_once(wm, run_ilia):
    app = run_ilia(wm.env, "-p", "actions")
    _wait(lambda: {"when:pass", "when:fail"} <= set(wm.lines()), 3, "both When commands")
    app.type("never")
    app.key("Return")
    _stays_open(app)
    for _ in "never":
        app.key("BackSpace")
    app.type("lights")
    app.key("Return")
    assert app.finish()[0] == 0
    assert "run:lights" in wm.lines()
    assert wm.lines().count("when:pass") == 1
    assert wm.lines().count("when:fail") == 1


def test_a_link_opens_the_submenu_and_backspace_climbs_out(wm, run_ilia):
    app = run_ilia(wm.env, "-p", "ACTIONS/system")
    app.key("Return")  # Suspend, the submenu's first row
    assert app.finish()[0] == 0
    assert "run:suspend 100%" in wm.lines()

    app = run_ilia(wm.env, "-p", "actions/system")
    app.key("BackSpace")  # System highlighted
    app.key("Up")
    app.key("Return")  # opens Capture
    app.key("Return")  # Grab a region
    assert app.finish()[0] == 0
    assert wm.runs() == ["exec fixture-grab --region"]


def test_links_that_name_no_submenu_fail_with_one_line(any_wm):
    env = any_wm.env
    cases = [
        ("actions/nope", "ilia: page 'actions' has no menu 'nope'\n"),
        ("actions/system/suspend", "ilia: page 'actions' has no menu 'system/suspend'\n"),
        ("actions/suspend", "ilia: page 'actions' has no menu 'suspend'\n"),
        ("apps/system", "ilia: page 'apps' has no menu 'system'\n"),
        ("unknown/system", "Unknown page type: unknown\nNo page loaded, exiting\n"),
    ]
    if any_wm.name != "sway":
        cases.append(
            ("actions/sway-only", "ilia: page 'actions' has no menu 'sway-only'\n"),
        )
    for link, message in cases:
        assert _run_without_window(env, "-p", link) == (1, "", message), link


def test_tabs_keep_the_menus_level_across_the_help_page(wm, run_ilia):
    app = run_ilia(wm.env, "-a", "-p", "actions/system")
    app.key("alt+h")
    app.key("alt+i")
    app.key("Return")  # still in System: Suspend
    assert app.finish()[0] == 0
    assert "run:suspend 100%" in wm.lines()


def test_state_commands_stop_after_two_seconds_and_run_again(wm, run_ilia):
    app = run_ilia(wm.env, "-p", "actions/system")
    _wait(lambda: wm.pids.exists() and wm.pids.read_text().strip(), 3, "the State command")
    first = int(wm.pids.read_text().split()[0])
    time.sleep(2.5)
    assert not _alive(first), "the State command outlived its 2 s"
    # About 5 s after it ended it runs again while its row is listed.
    _wait(lambda: len(wm.pids.read_text().split()) >= 2, 7, "the next State run")
    app.key("Escape")
    assert app.finish()[0] == 0
    for pid in map(int, wm.pids.read_text().split()):
        _wait(lambda: not _alive(pid), 2, f"State command {pid} to end")


@pytest.mark.parametrize("how", [signal.SIGTERM, signal.SIGKILL])
def test_state_commands_end_with_the_launcher_however_it_ends(no_wm, run_ilia, how):
    app = run_ilia(no_wm.env, "-p", "actions/system")
    _wait(lambda: no_wm.pids.exists() and no_wm.pids.read_text().strip(), 3, "the State command")
    pid = int(no_wm.pids.read_text().split()[0])
    assert _alive(pid)
    os.kill(app.proc.pid, how)
    app.proc.wait(timeout=5)
    assert app.proc.returncode == -how
    _wait(lambda: not _alive(pid), 2, "the State command to end with the launcher")


def test_a_file_with_a_repeated_id_or_a_loop_is_skipped_with_one_line(no_wm):
    env = no_wm.env
    for body, problem in (
        ("Entries=a;a\n[Entry a]\nName=A\nExec=x\n", "'a'"),
        ("Entries=a\n[Entry a]\nName=A\nEntries=b\n[Entry b]\nName=B\nEntries=a\n", "'a'"),
        # Lists of groups no chain reaches count as well.
        ("Entries=a\n[Entry a]\nName=A\nExec=x\n[Entry b]\nEntries=c\n[Entry c]\nEntries=b\n", "'"),
    ):
        no_wm.page.write_text("[Ilia Page]\nName=Actions\n" + body)
        code, stdout, stderr = _run_without_window(env, "-p", "actions")
        lines = stderr.splitlines()
        assert (code, stdout, len(lines)) == (1, "", 3), stderr
        assert str(no_wm.page) in lines[0] and problem in lines[0], lines[0]
        assert lines[1:] == ["Unknown page type: actions", "No page loaded, exiting"]
