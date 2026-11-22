"""Desktop entries read during start-up keep what GLib's own entries give.

Upstream #162 reads the desktop files on worker threads while GTK starts
(with no -p, -p apps or -a), and the port reads each file outside GLib so
that one stuck file cannot hold up the others. The entries built that way
must still be the ones GLib lists: a launch fills `%k` with the desktop
file, a `DBusActivatable=true` entry is activated over the session bus, and
the Windows page finds the entry GLib's search ranks first for a window's
app name, also one of several words joined by hyphens. Each case runs
against a fake i3 and a fake sway, alone on its page and with -a.
"""

import json
import struct
import subprocess
import sys
import time
import zlib
from contextlib import contextmanager
from pathlib import Path

import pytest

from parity.conftest import ROOT
from parity.fakes.executables import install_app_fakes
from parity.fakes.servers import I3Server

I3_CONFIG = json.loads((ROOT / "parity/fixtures/i3-config.json").read_text())
WMS = ["i3", "sway"]
APPS_MODES = {"apps": ("-p", "apps"), "all": ("-a",)}
WINDOWS_MODES = {"windows": ("-p", "windows"), "all": ("-a",)}


def _read(log, needle, timeout=5):
    deadline = time.monotonic() + timeout
    text = ""
    while time.monotonic() < deadline:
        text = log.read_text() if log.exists() else ""
        if needle in text:
            return text
        time.sleep(0.02)
    raise AssertionError(f"{needle!r} not in {text!r}")


def _window(node_id, title, app_name, wm):
    node = {
        "id": node_id, "type": "con", "urgent": False, "name": title,
        "window_type": "normal", "nodes": [], "floating_nodes": [],
    }
    if wm == "i3":
        node["window_properties"] = {
            "class": app_name.title(), "instance": app_name, "title": title,
            "machine": "fixture", "window_role": "",
        }
    else:
        node["app_id"] = app_name
    return node


@contextmanager
def _fake_wm(env, tmp_path, wm, windows=()):
    """A fake window manager answering the tree, version and config
    requests the pages send; its socket in the variable that wm uses."""
    socket_path = tmp_path / f"{wm}.sock"
    tree = {
        "id": 1, "type": "root", "urgent": False, "name": "root",
        "nodes": list(windows), "floating_nodes": [],
    }
    if wm == "i3":
        env["I3SOCK"] = str(socket_path)
        replies = {
            4: tree,
            7: {"human_readable": "4.25.1", "loaded_config_file_name": "/etc/i3/config",
                "major": 4, "minor": 25, "patch": 1},
            9: I3_CONFIG,
        }
    else:
        config = tmp_path / "sway-config"
        config.write_text("bindsym Mod4+Return exec fixture-sway\n")
        env.update({"I3SOCK": str(tmp_path / "stale-i3.sock"), "SWAYSOCK": str(socket_path)})
        replies = {
            4: tree,
            7: {"human_readable": "1.11", "loaded_config_file_name": str(config),
                "major": 1, "minor": 11, "patch": 0},
            9: {"config": config.read_text()},
        }
    with I3Server(socket_path, replies, tmp_path / f"{wm}-ipc.jsonl") as server:
        yield server


@pytest.mark.parametrize("mode", APPS_MODES)
@pytest.mark.parametrize("wm", WMS)
def test_field_code_k_names_the_desktop_file(isolated_env, run_ilia, tmp_path, wm, mode):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    probe = bindir / "fields-probe"
    probe.write_text(
        "#!/bin/sh\nset -eu\n"
        f'for arg in "$@"; do printf "arg:%s\\n" "$arg" >> "{log}"; done\n'
        f'printf "done\\n" >> "{log}"\n'
    )
    probe.chmod(0o755)
    entry = Path(env["XDG_DATA_HOME"]) / "applications" / "fields.desktop"
    entry.write_text(
        "[Desktop Entry]\nType=Application\nName=Fields Fixture\n"
        "Keywords=fieldcodes;\nIcon=fixture-icon\nExec=fields-probe %k %c %i\n"
    )
    with _fake_wm(env, tmp_path, wm):
        app = run_ilia(env, *APPS_MODES[mode])
        app.type("fieldcodes")
        app.key("Return")
        assert app.finish(timeout=8)[0] == 0
    lines = _read(log, "done").splitlines()
    assert lines[1:] == [
        f"arg:{entry}", "arg:Fields Fixture", "arg:--icon", "arg:fixture-icon", "done",
    ], lines


@pytest.mark.parametrize("mode", APPS_MODES)
@pytest.mark.parametrize("wm", WMS)
def test_dbus_activatable_entry_is_activated_over_the_bus(isolated_env, run_ilia, tmp_path, wm, mode):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    name = "org.example.HangbusX"
    entry = Path(env["XDG_DATA_HOME"]) / "applications" / f"{name}.desktop"
    entry.write_text(
        "[Desktop Entry]\nType=Application\nName=Bus Fixture\nKeywords=busfixture;\n"
        "DBusActivatable=true\nExec=fixture-app fallback-exec\n"
    )
    calls = tmp_path / "bus-app.log"
    service = subprocess.Popen(
        [sys.executable, "-m", "parity.fakes.bus_app", name, str(calls)],
        cwd=ROOT, env={**env, "PATH": "/usr/bin:/bin"},
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        assert service.stdout.readline().strip() == "ready", service.stderr.read()
        with _fake_wm(env, tmp_path, wm):
            app = run_ilia(env, *APPS_MODES[mode])
            app.type("busfixture")
            app.key("Return")
            assert app.finish(timeout=8)[0] == 0
        assert _read(calls, "Activate").splitlines() == ["Activate /org/example/HangbusX"]
    finally:
        service.terminate()
        service.wait(timeout=5)
    # The fallback Exec line never runs.
    assert "fallback-exec" not in (log.read_text() if log.exists() else "")


def _png(path, rgb, size=16):
    """Writes a square PNG of one opaque colour."""
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))
    row = b"\x00" + bytes(rgb) * size
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(row * size))
        + chunk(b"IEND", b"")
    )


CAPTURE = """
import sys
import gi
gi.require_version("Gdk", "3.0")
from gi.repository import Gdk
x, y, w, h = map(int, sys.argv[1:5])
pixbuf = Gdk.pixbuf_get_from_window(Gdk.get_default_root_window(), x, y, w, h)
data, n, stride = pixbuf.get_pixels(), pixbuf.get_n_channels(), pixbuf.get_rowstride()
counts = {"blue": 0, "green": 0, "red": 0}
for row in range(h):
    for col in range(w):
        r, g, b = data[row * stride + col * n:row * stride + col * n + 3]
        if b > 200 and r < 60 and g < 60:
            counts["blue"] += 1
        elif g > 200 and r < 60 and b < 60:
            counts["green"] += 1
        elif r > 200 and g < 60 and b < 60:
            counts["red"] += 1
print(" ".join(f"{k}={v}" for k, v in counts.items()))
"""


def _colours(app):
    geometry = subprocess.run(
        ["/usr/bin/xdotool", "getwindowgeometry", "--shell", app.window],
        env=app.env, text=True, capture_output=True, check=True,
    ).stdout
    values = dict(line.split("=", 1) for line in geometry.splitlines() if "=" in line)
    box = [values[key] for key in ("X", "Y", "WIDTH", "HEIGHT")]
    result = subprocess.run(
        [sys.executable, "-c", CAPTURE, *box], env=app.env, text=True, capture_output=True,
    )
    assert result.returncode == 0, result.stderr
    return dict(item.split("=") for item in result.stdout.split())


def _wait_colour(app, colour, timeout=6):
    deadline = time.monotonic() + timeout
    seen = {}
    while time.monotonic() < deadline:
        seen = {key: int(value) for key, value in _colours(app).items()}
        if seen[colour] >= 100:
            return seen
        time.sleep(0.1)
    raise AssertionError(f"no {colour} icon shown: {seen}")


@pytest.mark.parametrize("mode", WINDOWS_MODES)
@pytest.mark.parametrize("wm", WMS)
def test_window_icon_comes_from_the_entry_of_a_hyphenated_app_name(
    isolated_env, run_ilia, tmp_path, wm, mode,
):
    # GLib's search matches every word of the app name, in any order; an
    # entry sharing one word only is no match.
    env, _ = isolated_env
    icons = tmp_path / "icons"
    icons.mkdir()
    apps = Path(env["XDG_DATA_HOME"]) / "applications"
    apps.mkdir()
    for file_name, name, colour in (
        ("viewer.desktop", "HangimageX Viewer", (0, 0, 255)),
        ("tools.desktop", "HangimageX Tools", (255, 0, 0)),
        ("notes.desktop", "HangrevX Editor", (0, 255, 0)),
    ):
        icon = icons / f"{file_name}.png"
        _png(icon, colour)
        (apps / file_name).write_text(
            f"[Desktop Entry]\nType=Application\nName={name}\nIcon={icon}\nExec=/usr/bin/true\n"
        )
    windows = [
        _window(101, "Picture Window", "hangimagex-viewer", wm),
        _window(102, "Notes Window", "editor-hangrevx", wm),
    ]
    with _fake_wm(env, tmp_path, wm, windows):
        app = run_ilia(env, *WINDOWS_MODES[mode])
        if mode == "all":
            app.key("alt+w")
        app.type("Picture")
        seen = _wait_colour(app, "blue")
        assert seen["red"] == 0 and seen["green"] == 0, seen
        for _ in "Picture":
            app.key("BackSpace")
        app.type("Notes")
        seen = _wait_colour(app, "green")
        assert seen["red"] == 0 and seen["blue"] == 0, seen
        app.key("Escape")
        assert app.finish()[0] == 0
