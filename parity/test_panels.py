"""The Wi-Fi, Bluetooth and power panels (ported ahead of upstream #188).

python-dbusmock's templates stand in for NetworkManager, BlueZ and the
power-profiles daemon on the private bus, which is also the test's system
bus. The cases check what each template was asked: its call log, or the
property it holds afterwards.
"""

import importlib.util
import subprocess
import sys
import time

import pytest

from parity.conftest import _wait_for


GDBUS = "/usr/bin/gdbus"
MOCK_IFACE = "org.freedesktop.DBus.Mock"

pytestmark = pytest.mark.skipif(
    importlib.util.find_spec("dbusmock") is None, reason="python-dbusmock is not installed"
)


def _gdbus(env, *args):
    return subprocess.run(
        [GDBUS, *args], env=env, text=True, capture_output=True, timeout=10, check=True
    ).stdout


class Template:
    """One python-dbusmock template serving its bus name on the system bus."""

    def __init__(self, env, tmp_path, template, bus_name, path, mock_iface=MOCK_IFACE):
        self.env = env
        self.mock_iface = mock_iface
        self.bus_name = bus_name
        self.path = path
        self.log = tmp_path / f"{template}.log"
        self.proc = subprocess.Popen(
            [sys.executable, "-m", "dbusmock", "--system", "--template", template, "-l", str(self.log)],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
        )
        _gdbus(env, "wait", "--system", "--timeout", "8", bus_name)

    def mock(self, method, *args):
        return _gdbus(
            self.env, "call", "--system", "--dest", self.bus_name, "--object-path", self.path,
            "--method", f"{self.mock_iface}.{method}", *args,
        )

    def get(self, path, interface, name):
        return _gdbus(
            self.env, "call", "--system", "--dest", self.bus_name, "--object-path", path,
            "--method", "org.freedesktop.DBus.Properties.Get", interface, name,
        )

    def calls(self):
        return self.log.read_text() if self.log.exists() else ""

    def close(self):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


@pytest.fixture
def template(isolated_env, tmp_path):
    env, _bindir = isolated_env
    started = []

    def start(name, bus_name, path, mock_iface=MOCK_IFACE):
        mock = Template(env, tmp_path, name, bus_name, path, mock_iface)
        started.append(mock)
        return mock

    yield start
    for mock in started:
        mock.close()


@pytest.mark.parametrize("page", ["wifi", "bluetooth", "power", "WiFi"])
def test_panel_opens_without_its_service(isolated_env, run_ilia, page):
    env, _bindir = isolated_env
    app = run_ilia(env, "-p", page)
    time.sleep(0.5)
    assert app.proc.poll() is None
    app.key("Escape")
    code, stdout, stderr = app.finish()
    assert (code, stdout, stderr) == (0, "", "")


@pytest.mark.parametrize("page", ["wifi", "bluetooth", "power"])
def test_panels_are_not_in_all_pages_mode(isolated_env, run_ilia, page):
    env, _bindir = isolated_env
    app = run_ilia(env, "-a", "-p", page)
    app.key("Escape")
    code, stdout, stderr = app.finish()
    assert (code, stdout) == (0, "")
    # The Notifications page, loaded with -a, reports the missing
    # rofication socket on its own line.
    assert stderr.splitlines().count(f"Unknown page type: {page}") == 1
    assert "No page loaded" not in stderr


def test_power_enter_sets_the_highlighted_profile(isolated_env, run_ilia, template):
    env, _bindir = isolated_env
    daemon = template(
        "upower_power_profiles_daemon",
        "org.freedesktop.UPower.PowerProfiles",
        "/org/freedesktop/UPower/PowerProfiles",
    )
    assert "'balanced'" in daemon.get(daemon.path, "org.freedesktop.UPower.PowerProfiles", "ActiveProfile")
    app = run_ilia(env, "-p", "power")
    app.type("saver")
    _wait_for(lambda: "GetAll" in daemon.calls(), message="the profiles read")
    time.sleep(0.3)
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert (code, stdout, stderr) == (0, "", "")
    active = daemon.get(daemon.path, "org.freedesktop.UPower.PowerProfiles", "ActiveProfile")
    assert "'power-saver'" in active


def test_wifi_enter_joins_an_open_network(isolated_env, run_ilia, template):
    env, _bindir = isolated_env
    manager = template(
        "networkmanager", "org.freedesktop.NetworkManager", "/org/freedesktop/NetworkManager"
    )
    device = manager.mock("AddWiFiDevice", "mock_wlan", "wlan0", "30").strip()
    device = device.split("'")[1]
    manager.mock("AddAccessPoint", device, "ap_secured", "Home Net", "00:11:22:33:44:55",
                 "2", "2412", "54000", "0x50", "0x188")
    manager.mock("AddAccessPoint", device, "ap_guest", "Guest", "00:11:22:33:44:66",
                 "2", "2412", "54000", "0x30", "0")
    app = run_ilia(env, "-p", "wifi")
    _wait_for(lambda: "RequestScan" in manager.calls(), message="the scan request")
    app.type("guest")
    time.sleep(0.3)
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert (code, stdout, stderr) == (0, "", "")
    calls = manager.calls()
    assert "AddAndActivateConnection" in calls
    line = next(line for line in calls.splitlines() if "AddAndActivateConnection" in line)
    assert device in line and "/AccessPoint/ap_guest" in line
    assert "psk" not in line


def test_bluetooth_enter_connects_a_paired_device(isolated_env, run_ilia, template):
    env, _bindir = isolated_env
    bluez = template("bluez5", "org.bluez", "/", "org.bluez.Mock")
    bluez.mock("AddAdapter", "hci0", "my-computer")
    bluez.mock("AddDevice", "hci0", "11:22:33:44:55:66", "Headphones")
    bluez.mock("PairDevice", "hci0", "11:22:33:44:55:66")
    app = run_ilia(env, "-p", "bluetooth")
    _wait_for(lambda: "GetManagedObjects" in bluez.calls(), message="the objects read")
    time.sleep(0.3)
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert (code, stdout, stderr) == (0, "", "")
    # The template's Connect announces the change but leaves its own
    # property as it was, so the calls tell what ilia asked.
    methods = [line.split()[1] for line in bluez.calls().splitlines() if len(line.split()) > 1]
    assert "Connect" in methods, bluez.calls()
    assert "Pair" not in methods and "Disconnect" not in methods
