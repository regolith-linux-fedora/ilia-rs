import os
import select
import subprocess
import time
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("ILIA_BINARY", ROOT / "build/src/ilia"))


def _wait_for(predicate, timeout=5, message="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise TimeoutError(f"timed out waiting for {message}")


@pytest.fixture(scope="session")
def display(tmp_path_factory):
    proc = None
    value = None
    for offset in range(500):
        number = 90 + (os.getpid() + offset) % 500
        socket = Path(f"/tmp/.X11-unix/X{number}")
        lock = Path(f"/tmp/.X{number}-lock")
        if socket.exists() or lock.exists():
            continue
        candidate = subprocess.Popen(
            ["Xvfb", f":{number}", "-screen", "0", "1024x768x24", "-ac", "-noreset", "-nolisten", "tcp"],
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        _wait_for(
            lambda: socket.exists() or candidate.poll() is not None,
            message="Xvfb",
        )
        if candidate.poll() is None and socket.exists():
            proc = candidate
            value = f":{number}"
            break
        candidate.wait(timeout=3)
    if proc is None or value is None:
        raise RuntimeError("could not allocate a private Xvfb display")
    try:
        yield value
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()


@pytest.fixture
def isolated_env(tmp_path, display):
    schema = tmp_path / "schemas"
    schema.mkdir()
    xml = (ROOT / "data/org.regolith-linux.ilia.gschema.xml").read_text()
    # Memory backends cannot be pre-seeded from a separate process. Compile an
    # otherwise identical schema with small, safe defaults in the isolated dir.
    xml = xml.replace("<default>32</default>", "<default>16</default>")
    xml = xml.replace("<default>100000</default>", "<default>1</default>")
    (schema / "org.regolith-linux.ilia.gschema.xml").write_text(xml)
    subprocess.run(["glib-compile-schemas", str(schema)], check=True)

    home = tmp_path / "home"
    data = tmp_path / "data"
    config = tmp_path / "config"
    cache = tmp_path / "cache"
    runtime = tmp_path / "runtime"
    bindir = tmp_path / "bin"
    for path in (home, data, config, cache, runtime, bindir):
        path.mkdir()
    # Keep application discovery isolated while making GTK's immutable icon
    # and MIME databases available through the isolated data root.
    (data / "icons").symlink_to("/usr/share/icons", target_is_directory=True)
    (data / "mime").symlink_to("/usr/share/mime", target_is_directory=True)
    # GdkPixbuf 2.44 loads images through glycin, whose loader configuration
    # is found through XDG_DATA_DIRS as well; glycin runs its loaders under bwrap, found through PATH.
    glycin = Path("/usr/share/glycin-loaders")
    if glycin.is_dir():
        (data / "glycin-loaders").symlink_to(glycin, target_is_directory=True)
        if Path("/usr/bin/bwrap").exists():
            (bindir / "bwrap").symlink_to("/usr/bin/bwrap")

    bus_tmp = Path(os.environ.get("TMPDIR", tmp_path))
    bus_config = tmp_path / "session-bus.conf"
    bus_config.write_text(
        '<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN" '
        '"http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">\n'
        f'<busconfig><type>session</type><listen>unix:tmpdir={bus_tmp}</listen>'
        '<auth>EXTERNAL</auth><policy context="default">'
        '<allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/>'
        '</policy></busconfig>\n'
    )
    daemon = subprocess.Popen(
        ["dbus-daemon", f"--config-file={bus_config}", "--nofork", "--print-address=1"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    ready, _, _ = select.select([daemon.stdout], [], [], 5)
    if not ready:
        daemon.terminate()
        daemon.wait(timeout=2)
        raise TimeoutError("timed out waiting for private D-Bus address")
    address = daemon.stdout.readline().strip()
    assert address.startswith("unix:"), address

    env = os.environ.copy()
    env.update({
        "DISPLAY": display,
        "GDK_BACKEND": "x11",
        "XDG_SESSION_TYPE": "x11",
        "NO_AT_BRIDGE": "1",
        "GSETTINGS_BACKEND": "memory",
        "GSETTINGS_SCHEMA_DIR": str(schema),
        "HOME": str(home),
        "XDG_DATA_HOME": str(data),
        "XDG_DATA_DIRS": str(data),
        "XDG_CONFIG_HOME": str(config),
        "XDG_CACHE_HOME": str(cache),
        "XDG_RUNTIME_DIR": str(runtime),
        "DBUS_SESSION_BUS_ADDRESS": address,
        # ilia watches the system bus for a suspend; the private bus stands in
        # for it, so no host service is reached.
        "DBUS_SYSTEM_BUS_ADDRESS": address,
        "PATH": str(bindir),
    })
    env.pop("I3SOCK", None)
    env.pop("SWAYSOCK", None)
    try:
        yield env, bindir
    finally:
        daemon.terminate()
        try:
            daemon.wait(timeout=2)
        except subprocess.TimeoutExpired:
            daemon.kill()


class IliaProcess:
    def __init__(self, env, *args, stdin=""):
        self.env = env
        self.proc = subprocess.Popen(
            [str(BINARY), *args], env=env, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        if stdin:
            self.proc.stdin.write(stdin)
        self.proc.stdin.close()
        self.proc.stdin = None
        self.window = _wait_for(self._find_window, timeout=8, message="Ilia window")

    def _find_window(self):
        if self.proc.poll() is not None:
            stdout, stderr = self.proc.communicate()
            raise AssertionError(
                f"Ilia exited before creating a window: {self.proc.returncode}; "
                f"stdout={stdout!r} stderr={stderr!r}"
            )
        result = subprocess.run(
            ["/usr/bin/xdotool", "search", "--pid", str(self.proc.pid)],
            env=self.env, text=True, capture_output=True,
        )
        if result.returncode == 0 and result.stdout:
            window = self._largest_window(result.stdout.splitlines())
            if window:
                return window
        # GTK popup windows do not reliably publish _NET_WM_PID without a
        # window manager. Each test owns its X display and starts one window.
        result = subprocess.run(
            ["/usr/bin/xdotool", "search", "--onlyvisible", "--class", "ilia"],
            env=self.env, text=True, capture_output=True,
        )
        return self._largest_window(result.stdout.splitlines()) if result.returncode == 0 and result.stdout else None

    def _largest_window(self, windows):
        best = None
        best_area = 0
        for window in windows:
            result = subprocess.run(
                ["/usr/bin/xdotool", "getwindowgeometry", "--shell", window],
                env=self.env, text=True, capture_output=True,
            )
            values = dict(
                line.split("=", 1) for line in result.stdout.splitlines() if "=" in line
            )
            area = int(values.get("WIDTH", 0)) * int(values.get("HEIGHT", 0))
            if area > best_area:
                best, best_area = window, area
        return best if best_area >= 10_000 else None

    def type(self, text):
        subprocess.run(["/usr/bin/xdotool", "windowfocus", self.window], env=self.env, check=True)
        subprocess.run(["/usr/bin/xdotool", "type", "--delay", "5", "--", text], env=self.env, check=True)

    def key(self, key):
        subprocess.run(["/usr/bin/xdotool", "windowfocus", self.window], env=self.env, check=True)
        subprocess.run(["/usr/bin/xdotool", "key", "--clearmodifiers", key], env=self.env, check=True)

    def finish(self, timeout=5):
        try:
            stdout, stderr = self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            stdout, stderr = self.proc.communicate()
            raise AssertionError(f"Ilia did not exit; stdout={stdout!r} stderr={stderr!r}")
        return self.proc.returncode, stdout, stderr

    def close(self):
        if self.proc.poll() is None:
            self.key("Escape")
            self.finish()


@pytest.fixture
def run_ilia():
    sessions = []

    def run(env, *args, stdin=""):
        session = IliaProcess(env, *args, stdin=stdin)
        sessions.append(session)
        return session

    yield run
    for session in sessions:
        session.close()
