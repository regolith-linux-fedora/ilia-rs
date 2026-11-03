#!/bin/bash
# Cutover probe: the four final pages, the remaining dialog shell, and the
# meson-built/installed Rust binary, each under an isolated headless session.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
binary=${ILIA_BINARY:-$root/rust/target/release/ilia}
source_root=${ILIA_SOURCE_ROOT:-$root}
[[ -x "$binary" ]] || { printf 'FAIL: ilia binary not found: %s\n' "$binary" >&2; exit 1; }
[[ -f "$source_root/parity/fakes/servers.py" ]] || {
    printf 'FAIL: parity fakes not found under %s\n' "$source_root" >&2
    exit 1
}
binary=$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")
rofication_socket=/tmp/rofi_notification_daemon

tmp=$(mktemp -d)
kill_tree() {
    local pid=$1 child
    for child in $(pgrep -P "$pid" 2>/dev/null || true); do
        kill_tree "$child"
    done
    kill "$pid" 2>/dev/null || true
}
cleanup() {
    [[ -n "${dbus_pid:-}" ]] && kill_tree "$dbus_pid"
    [[ -n "${xvfb_pid:-}" ]] && kill "$xvfb_pid" 2>/dev/null
    wait 2>/dev/null || true
    rm -f "$rofication_socket"
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

Xvfb -displayfd 3 -screen 0 1024x768x24 -ac -noreset -nolisten tcp \
    3>"$tmp/display" >"$tmp/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in $(seq 1 100); do
    [[ -s "$tmp/display" ]] && break
    kill -0 "$xvfb_pid" 2>/dev/null || {
        cat "$tmp/xvfb.log" >&2
        exit 1
    }
    sleep .05
done
[[ -s "$tmp/display" ]]
read -r display_number <"$tmp/display"
export DISPLAY=":$display_number" GDK_BACKEND=x11 XDG_SESSION_TYPE=x11 NO_AT_BRIDGE=1
export GSETTINGS_BACKEND=memory HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/config"
export XDG_DATA_HOME="$tmp/data" XDG_CACHE_HOME="$tmp/cache" XDG_RUNTIME_DIR="$tmp/runtime"
export TMPDIR="$tmp/tmp"
mkdir -p "$HOME" "$TMPDIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
mkdir -p "$tmp/schemas"
cp "$source_root/data/org.regolith-linux.ilia.gschema.xml" "$tmp/schemas/"
glib-compile-schemas "$tmp/schemas"
export GSETTINGS_SCHEMA_DIR="$tmp/schemas"
unset SWAYSOCK I3SOCK GTK_MODULES DBUS_SESSION_BUS_ADDRESS

# Private session bus. The only activatable service is the accessibility bus
# (for the AT-SPI page-tab probe), so no Tracker miner can ever appear on it.
mkdir -p "$tmp/bus" "$tmp/dbus-services"
cp /usr/share/dbus-1/services/org.a11y.Bus.service "$tmp/dbus-services/"
cat >"$tmp/session-bus.conf" <<EOF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig><type>session</type><listen>unix:tmpdir=$tmp/bus</listen>
<auth>EXTERNAL</auth><servicedir>$tmp/dbus-services</servicedir>
<policy context="default"><allow send_destination="*"/><allow receive_sender="*"/>
<allow own="*"/></policy></busconfig>
EOF
dbus-daemon --config-file="$tmp/session-bus.conf" --nofork --print-address=3 \
    3>"$tmp/bus-address" >"$tmp/dbus.log" 2>&1 &
dbus_pid=$!
for _ in $(seq 1 100); do
    [[ -s "$tmp/bus-address" ]] && break
    sleep .05
done
read -r bus_address <"$tmp/bus-address"
[[ "$bus_address" == unix:* ]]
export DBUS_SESSION_BUS_ADDRESS=$bus_address

cat >"$tmp/driver.py" <<'PY'
import json
import os
import subprocess
import sys
import time
from pathlib import Path

SOURCE = Path(os.environ["SMOKE_SOURCE"])
TMP = Path(os.environ["SMOKE_TMP"])
BINARY = os.environ["SMOKE_BINARY"]
FIXTURES = SOURCE / "parity/fixtures"
XDOTOOL = "/usr/bin/xdotool"
ATSPI = os.environ.get("ILIA_ATSPI_SNAPSHOT", str(SOURCE / "rust/atspi-snapshot.py"))
SOCKET = Path("/tmp/rofi_notification_daemon")
sys.path.insert(0, str(SOURCE))
from parity.fakes.executables import install_terminal_fakes  # noqa: E402
from parity.fakes.servers import RoficationServer  # noqa: E402

TOOL_ENV = dict(os.environ)
LIVE = []


class Failure(Exception):
    pass


def check(condition, message):
    if not condition:
        raise Failure(message)


def wait_for(predicate, timeout, what):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise Failure(f"timed out waiting for {what}")


def xdotool(*args, check_ok=True):
    result = subprocess.run([XDOTOOL, *args], env=TOOL_ENV, text=True, capture_output=True)
    if check_ok and result.returncode != 0:
        raise Failure(f"xdotool {' '.join(args)} failed: {result.stderr.strip()}")
    return result


def geometry(window):
    result = xdotool("getwindowgeometry", "--shell", window)
    values = dict(line.split("=", 1) for line in result.stdout.splitlines() if "=" in line)
    return int(values.get("WIDTH", 0)), int(values.get("HEIGHT", 0))


def case_env(name, backend="memory", atspi=False):
    base = TMP / "cases" / name
    dirs = {key: base / key for key in ("home", "data", "config", "cache", "runtime", "bin")}
    for path in dirs.values():
        path.mkdir(parents=True)
    dirs["runtime"].chmod(0o700)
    (dirs["data"] / "icons").symlink_to("/usr/share/icons", target_is_directory=True)
    (dirs["data"] / "mime").symlink_to("/usr/share/mime", target_is_directory=True)
    # GdkPixbuf 2.44 loads images through glycin, whose loader configuration
    # is found through XDG_DATA_DIRS as well; glycin runs its loaders under
    # bwrap, found through PATH.
    if Path("/usr/share/glycin-loaders").is_dir():
        (dirs["data"] / "glycin-loaders").symlink_to("/usr/share/glycin-loaders", target_is_directory=True)
        if Path("/usr/bin/bwrap").exists():
            (dirs["bin"] / "bwrap").symlink_to("/usr/bin/bwrap")
    env = dict(os.environ)
    for key in ("I3SOCK", "SWAYSOCK", "GTK_MODULES", "SMOKE_SOURCE", "SMOKE_TMP", "SMOKE_BINARY"):
        env.pop(key, None)
    env.update({
        "GSETTINGS_BACKEND": backend,
        "HOME": str(dirs["home"]),
        "XDG_DATA_HOME": str(dirs["data"]),
        "XDG_DATA_DIRS": str(dirs["data"]),
        "XDG_CONFIG_HOME": str(dirs["config"]),
        "XDG_CACHE_HOME": str(dirs["cache"]),
        "XDG_RUNTIME_DIR": str(dirs["runtime"]),
        "TMPDIR": str(base),
        "PATH": str(dirs["bin"]),
        "NO_AT_BRIDGE": "1",
    })
    if atspi:
        env.pop("NO_AT_BRIDGE")
        env["GTK_MODULES"] = "gail:atk-bridge"
    return env, base, dirs["bin"]


class Ilia:
    def __init__(self, env, *args, stdin="", binary=None):
        self.env = env
        self.proc = subprocess.Popen(
            [binary or BINARY, *args], env=env, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        LIVE.append(self.proc)
        if stdin:
            self.proc.stdin.write(stdin)
        self.proc.stdin.close()
        self.proc.stdin = None
        self.window = wait_for(self._find_window, 8, f"ilia window ({' '.join(args)})")

    def _largest(self, windows):
        best, best_area = None, 0
        for window in windows:
            width, height = geometry(window)
            if width * height > best_area:
                best, best_area = window, width * height
        return best if best_area >= 10_000 else None

    def _find_window(self):
        if self.proc.poll() is not None:
            stdout, stderr = self.proc.communicate()
            raise Failure(
                f"ilia exited before creating a window: {self.proc.returncode}; "
                f"stdout={stdout!r} stderr={stderr!r}"
            )
        result = xdotool("search", "--pid", str(self.proc.pid), check_ok=False)
        if result.returncode == 0 and result.stdout:
            window = self._largest(result.stdout.split())
            if window:
                return window
        result = xdotool("search", "--onlyvisible", "--class", "ilia", check_ok=False)
        if result.returncode == 0 and result.stdout:
            return self._largest(result.stdout.split())
        return None

    def type(self, text):
        xdotool("windowfocus", self.window)
        xdotool("type", "--delay", "5", "--", text)

    def key(self, key):
        xdotool("windowfocus", self.window)
        xdotool("key", "--clearmodifiers", key)

    def running(self):
        return self.proc.poll() is None

    def finish(self, timeout=5):
        try:
            stdout, stderr = self.proc.communicate(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            stdout, stderr = self.proc.communicate()
            raise Failure(f"ilia did not exit; stdout={stdout!r} stderr={stderr!r}")
        return self.proc.returncode, stdout, stderr

    def escape(self):
        self.key("Escape")
        return self.finish()


def expect_clean_escape(app, what):
    code, stdout, stderr = app.escape()
    check(code == 0, f"{what}: Escape exit status {code}; stderr={stderr!r}")
    check(stdout == "", f"{what}: unexpected stdout {stdout!r}")
    return stderr


def log_text(log):
    return log.read_text() if log.exists() else ""


# --- terminal -------------------------------------------------------------

def case_terminal_quiet():
    env, base, bindir = case_env("terminal-quiet")
    log = install_terminal_fakes(bindir, base)
    app = Ilia(env, "-p", "terminal", "-q")
    app.type("fixture-command")
    app.key("Return")
    code, _, stderr = app.finish()
    check(code == 0, f"exit status {code}; stderr={stderr!r}")
    lines = wait_for(lambda: "fixture:" in log_text(log) and log_text(log), 2,
                     "fixture command transcript").splitlines()
    check(len(lines) == 2, f"transcript {lines!r}")
    check(lines[0].startswith("systemd:--user --scope --unit run_ilia_"), f"wrapper line {lines[0]!r}")
    check(lines[0].endswith(f" {bindir}/fixture-command"), f"wrapper target {lines[0]!r}")
    check(lines[1] == "fixture:", f"command line {lines[1]!r}")


def case_terminal_wrapper():
    env, base, bindir = case_env("terminal-wrapper")
    log = install_terminal_fakes(bindir, base)
    app = Ilia(env, "-p", "terminal")
    app.type("fixture-command")
    app.key("Return")
    code, _, stderr = app.finish()
    check(code == 0, f"exit status {code}; stderr={stderr!r}")
    lines = wait_for(lambda: "terminal:" in log_text(log) and log_text(log), 2,
                     "terminal transcript").splitlines()
    check(len(lines) == 2, f"transcript {lines!r}")
    check(lines[0].startswith("systemd:--user --scope --unit run_ilia_"), f"wrapper line {lines[0]!r}")
    check(lines[0].endswith(f" x-terminal-emulator -e {bindir}/fixture-command"),
          f"wrapper target {lines[0]!r}")
    check(lines[1] == f"terminal:-e {bindir}/fixture-command", f"terminal line {lines[1]!r}")


def case_terminal_unmatched():
    # Enter on a command line that matches no listed command runs it as typed.
    env, base, bindir = case_env("terminal-unmatched")
    log = install_terminal_fakes(bindir, base)
    app = Ilia(env, "-p", "terminal", "-q")
    app.type("free-command hello")
    app.key("Return")
    code, _, stderr = app.finish()
    check(code == 0, f"exit status {code}; stderr={stderr!r}")
    lines = wait_for(lambda: "free:" in log_text(log) and log_text(log), 2,
                     "typed command transcript").splitlines()
    check(len(lines) == 2, f"transcript {lines!r}")
    check(lines[0].startswith("systemd:--user --scope --unit run_ilia_"), f"wrapper line {lines[0]!r}")
    check(lines[0].endswith(" free-command hello"), f"wrapper target {lines[0]!r}")
    check(lines[1] == "free:hello", f"command line {lines[1]!r}")


# --- textlist -------------------------------------------------------------

def textlist_select(name, args, stdin, query, expected):
    env, _, _ = case_env(name)
    app = Ilia(env, "-p", "textlist", *args, stdin=stdin)
    if query:
        app.type(query)
    app.key("Return")
    code, stdout, stderr = app.finish()
    check(code == 0, f"exit status {code}; stderr={stderr!r}")
    check(stdout == expected, f"stdout {stdout!r}, expected {expected!r}")
    check("No page loaded" not in stderr, f"stderr {stderr!r}")


def case_textlist_selection():
    items = (FIXTURES / "textlist-items.txt").read_text()
    textlist_select("textlist-select", ["-n"], items, None, "Alpha\n")


def case_textlist_filtered():
    items = (FIXTURES / "textlist-items.txt").read_text()
    textlist_select("textlist-filtered", ["-n"], items, "needle", "Needle Item\n")


def case_textlist_label_icon():
    textlist_select("textlist-label-icon", ["-l", "Fixture", "-i", "applications-other"],
                    "Only\n", None, "Only\n")


def case_textlist_unmatched():
    # Enter on text that matches no row prints the typed text.
    textlist_select("textlist-unmatched", ["-n"], (FIXTURES / "textlist-items.txt").read_text(),
                    "unmatched free text", "unmatched free text\n")


# --- notifications --------------------------------------------------------

def requests(server):
    return [json.loads(line)["request"] for line in server.transcript.read_text().splitlines()]


def wait_request(server, prefix):
    return wait_for(lambda: ([r for r in requests(server) if r.startswith(prefix)] or [None])[-1],
                    3, f"rofication {prefix!r} request (have {requests(server)!r})")


def with_rofication(name, body):
    env, base, _ = case_env(name)
    data = json.loads((FIXTURES / "notifications.json").read_text())
    with RoficationServer(SOCKET, data, base / "rofication.jsonl") as server:
        app = Ilia(env, "-p", "notifications")
        try:
            wait_request(server, "list")
            body(app, server)
            code, _, stderr = app.escape()
            check(code == 0, f"Escape exit status {code}; stderr={stderr!r}")
        finally:
            if app.running():
                app.proc.kill()
                app.proc.wait()
    check(not SOCKET.exists(), "rofication socket not removed")


def case_notifications_delete():
    def body(app, server):
        app.key("Delete")
        check(wait_request(server, "del:") == "del:11\n", f"transcript {requests(server)!r}")
        wait_for(lambda: len(requests(server)) >= 3, 3, f"reload list (have {requests(server)!r})")
        time.sleep(0.2)
        check(requests(server) == ["list\n", "del:11\n", "list\n"], f"transcript {requests(server)!r}")
    with_rofication("notifications-delete", body)


def multi_delete(name, query, keep, drop):
    def body(app, server):
        app.type(query)
        app.key("shift+Delete")
        request = wait_request(server, "delm:")
        check(request.endswith("\n"), f"delm framing {request!r}")
        ids = set(request[len("delm:"):-1].split(","))
        check(keep in ids and drop not in ids, f"{query!r} deleted {request!r}")
        check(ids == {keep}, f"{query!r} deleted {request!r}")
    with_rofication(name, body)


def case_notifications_app_filter():
    multi_delete("notifications-app-filter", "Messenger", "22", "11")


def case_notifications_body_filter():
    multi_delete("notifications-body-filter", "alpha body", "11", "22")


# --- tracker --------------------------------------------------------------

def case_tracker_no_miner():
    env, _, _ = case_env("tracker")
    app = Ilia(env, "-p", "tracker")
    app.type("fixture query")
    time.sleep(0.3)
    check(app.running(), "tracker failure quit the dialog")
    stderr = expect_clean_escape(app, "tracker")
    check("org.freedesktop.Tracker3.Miner.Files" in stderr, f"stderr {stderr!r}")


# --- stylesheet modes -----------------------------------------------------

def case_stylesheet_missing():
    env, base, _ = case_env("style-missing")
    missing = base / "missing.css"
    result = subprocess.run([BINARY, "-p", "apps", "-t", str(missing)], env=env,
                            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
    check(result.returncode == 1, f"exit status {result.returncode}")
    check(result.stdout == "", f"stdout {result.stdout!r}")
    check(result.stderr == f"File '{missing}' does not exist.\n", f"stderr {result.stderr!r}")


def style_startup(name, args):
    env, base, _ = case_env(name)
    css = base / "style.css"
    css.write_text("#root_box { margin: 4px; }\nwindow { border: 2px solid #336699; }\n")
    args = [str(css) if arg == "@CSS@" else arg for arg in args]
    app = Ilia(env, "-p", "apps", *args)
    stderr = expect_clean_escape(app, name)
    check(stderr == "", f"stderr {stderr!r}")


def case_stylesheet_file():
    style_startup("style-file", ["-t", "@CSS@"])


def case_stylesheet_none():
    style_startup("style-none", ["-n"])


def case_stylesheet_default():
    style_startup("style-default", [])


# --- Alt switching (AT-SPI) -----------------------------------------------

def page_tabs(node, found):
    if node.get("role") == "page tab":
        found.append(node)
    for child in node.get("children", []):
        page_tabs(child, found)
    return found


def selected_tab():
    result = subprocess.run(["/usr/bin/python3", ATSPI], env=TOOL_ENV,
                            capture_output=True, text=True, timeout=20)
    if result.returncode != 0:
        raise Failure(f"AT-SPI snapshot failed: {result.stderr.strip()}")
    tabs = page_tabs(json.loads(result.stdout), [])
    selected = [tab["name"] for tab in tabs if tab["selected"]]
    return [tab["name"] for tab in tabs], selected


def case_alt_switch():
    env, _, _ = case_env("alt-switch", atspi=True)
    app = Ilia(env, "-a", "-p", "apps")
    tabs, selected = selected_tab()
    check(any("Windows" in name for name in tabs), f"no Windows tab in {tabs!r}")
    check(len(selected) == 1 and "Windows" not in selected[0], f"initial selection {selected!r} of {tabs!r}")
    initial = selected[0]
    app.key("alt+w")
    state = {}

    def switched():
        state["tabs"], state["selected"] = selected_tab()
        return state["selected"] == ["Windows"]

    try:
        wait_for(switched, 8, "Windows tab selection")
    except Failure:
        raise Failure(f"after alt+w selection {state.get('selected')!r} (was {initial!r})")
    check(app.running(), "alt+w quit the dialog")
    code, stdout, stderr = app.escape()
    check(code == 0, f"Escape exit status {code}; stderr={stderr!r}")


# --- Alt+plus / Alt+minus resize ------------------------------------------

def read_keyfile(path):
    values = {}
    if path.exists():
        for line in path.read_text().splitlines():
            if "=" in line:
                key, value = line.split("=", 1)
                values[key.strip()] = value.strip()
    return values


def case_resize():
    env, _, _ = case_env("resize", backend="keyfile")
    keyfile = Path(env["XDG_CONFIG_HOME"]) / "glib-2.0/settings/keyfile"
    app = Ilia(env, "-p", "apps")
    width, height = wait_for(lambda: geometry(app.window), 3, "window geometry")
    for key, delta in (("alt+plus", 128), ("alt+minus", 0)):
        want = (width + delta, height + delta)
        app.key(key)
        try:
            wait_for(lambda: geometry(app.window) == want, 3, f"{key} geometry")
        except Failure:
            raise Failure(f"{key}: geometry {geometry(app.window)} (start {width}x{height}), expected {want}")
        expected = {"window-width": str(want[0]), "window-height": str(want[1])}
        try:
            wait_for(lambda: all(read_keyfile(keyfile).get(k) == v for k, v in expected.items()),
                     3, f"{key} persistence")
        except Failure:
            raise Failure(f"{key}: keyfile {read_keyfile(keyfile)!r}, expected {expected!r}")
        check(app.running(), f"{key} quit the dialog")
    code, stdout, stderr = app.escape()
    check(code == 0, f"Escape exit status {code}; stderr={stderr!r}")


# --- focus loss -----------------------------------------------------------

HELPER = """
import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gtk
window = Gtk.Window(title="ilia-smoke-focus-helper")
window.set_default_size(40, 40)
window.move(0, 0)
window.connect("destroy", Gtk.main_quit)
window.show_all()
Gtk.main()
"""


def case_focus_loss():
    # While ilia's seat grab is active the X server reports focus moves as
    # NotifyWhileGrabbed, which GDK (Vala reference and port alike) does not
    # turn into focus-out. Move focus to another window, then break the grab
    # with the XKB XF86Ungrab action: the server then delivers the pending
    # FocusOut (NotifyUngrab) and the dialog must quit on its own.
    result = subprocess.run(["/usr/bin/setxkbmap", "-option", "grab:break_actions"],
                            env=TOOL_ENV, capture_output=True, text=True)
    check(result.returncode == 0, f"setxkbmap failed: {result.stderr.strip()}")
    env, _, _ = case_env("focus-loss")
    app = Ilia(env, "-p", "apps")
    helper_env = dict(TOOL_ENV)
    helper_env["NO_AT_BRIDGE"] = "1"
    # Run the helper from a file: process tracers record argv one line per
    # argument, so keep every argument single-line.
    helper_script = TMP / "focus-helper.py"
    helper_script.write_text(HELPER)
    helper = subprocess.Popen(["/usr/bin/python3", str(helper_script)], env=helper_env,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    LIVE.append(helper)
    try:
        other = wait_for(
            lambda: (xdotool("search", "--onlyvisible", "--name", "^ilia-smoke-focus-helper$",
                             check_ok=False).stdout.split() or [None])[0],
            8, "focus helper window")
        time.sleep(0.2)
        check(app.running(), "ilia quit before focus moved")
        xdotool("windowfocus", other)
        time.sleep(0.2)
        xdotool("key", "ctrl+alt+KP_Divide")
        code, stdout, stderr = app.finish(timeout=5)
        check(code == 0, f"exit status {code}; stderr={stderr!r}")
        check(stdout == "", f"stdout {stdout!r}")
    finally:
        helper.terminate()
        helper.wait(timeout=5)


# --- meson-built binary ---------------------------------------------------

def case_meson_start():
    env, _, _ = case_env(os.environ.get("SMOKE_MESON_LABEL", "meson-start"))
    app = Ilia(env, "-p", "apps", binary=os.environ["SMOKE_MESON_BINARY"])
    expect_clean_escape(app, "meson-built binary")


GROUPS = {
    "pages": [
        ("terminal-quiet", case_terminal_quiet),
        ("terminal-x-terminal-emulator", case_terminal_wrapper),
        ("terminal-unmatched-enter", case_terminal_unmatched),
        ("textlist-selection", case_textlist_selection),
        ("textlist-filtered-selection", case_textlist_filtered),
        ("textlist-label-icon", case_textlist_label_icon),
        ("textlist-unmatched-enter", case_textlist_unmatched),
        ("notifications-single-delete", case_notifications_delete),
        ("notifications-application-filter-delete-many", case_notifications_app_filter),
        ("notifications-body-filter-delete-many", case_notifications_body_filter),
        ("tracker-no-miner", case_tracker_no_miner),
        ("stylesheet-missing-file", case_stylesheet_missing),
        ("stylesheet-file", case_stylesheet_file),
        ("stylesheet-none", case_stylesheet_none),
        ("stylesheet-default", case_stylesheet_default),
        ("alt-letter-page-switch", case_alt_switch),
        ("alt-plus-minus-resize-persistence", case_resize),
        ("focus-loss-quit", case_focus_loss),
    ],
    "meson-start": [("meson-built-binary-start", case_meson_start)],
}


def main():
    for name, case in GROUPS[sys.argv[1]]:
        try:
            case()
        except Failure as error:
            print(f"FAIL: {name}: {error}", file=sys.stderr, flush=True)
            return 1
        finally:
            for proc in LIVE:
                if proc.poll() is None:
                    proc.kill()
                    proc.wait()
            LIVE.clear()
        print(f"ok: {name}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
PY

driver() {
    SMOKE_SOURCE="$source_root" SMOKE_TMP="$tmp" SMOKE_BINARY="$binary" \
        PYTHONDONTWRITEBYTECODE=1 python3 "$tmp/driver.py" "$@"
}
driver pages

# Meson-built binary: configure, compile (cargo --locked), start it, then stage an install and check layout.
build="$tmp/meson-build"
stage="$tmp/stage"
if ! { meson setup "$build" "$source_root" && meson compile -C "$build"; } >"$tmp/meson.log" 2>&1; then
    tail -n 60 "$tmp/meson.log" >&2
    printf 'FAIL: meson-build: setup/compile failed\n' >&2
    exit 1
fi
[[ -x "$build/src/ilia" ]] || { printf 'FAIL: meson-build: no %s\n' "$build/src/ilia" >&2; exit 1; }
SMOKE_MESON_BINARY="$build/src/ilia" driver meson-start
# The checkout's own meson build (`meson setup build && meson compile -C
# build`), when present, is the shipped build/src/ilia: start it too.
if [[ -x "$source_root/build/src/ilia" ]]; then
    SMOKE_MESON_LABEL=meson-start-checkout SMOKE_MESON_BINARY="$source_root/build/src/ilia" \
        driver meson-start
fi

touch "$tmp/install-marker"
sleep 1
if ! DESTDIR="$stage" meson install -C "$build" >"$tmp/install.log" 2>&1; then
    cat "$tmp/install.log" >&2
    printf 'FAIL: meson-install: staged install failed\n' >&2
    exit 1
fi
fail_install() {
    printf 'FAIL: meson-install-layout: %s\n' "$1" >&2
    (cd "$stage" && find . | sort) >&2
    exit 1
}
prefix="$stage/usr/local"
[[ -x "$prefix/bin/ilia" ]] || fail_install "missing usr/local/bin/ilia"
cmp -s "$prefix/bin/ilia" "$build/src/ilia" || fail_install "installed ilia differs from build/src/ilia"
schema="$prefix/share/glib-2.0/schemas/org.regolith-linux.ilia.gschema.xml"
cmp -s "$schema" "$source_root/data/org.regolith-linux.ilia.gschema.xml" ||
    fail_install "missing or different gschema XML"
[[ -n "$(find "$prefix/share/ilia" -type f -print -quit 2>/dev/null)" ]] ||
    fail_install "no files under usr/local/share/ilia"
[[ -n "$(find "$prefix/share/doc/ilia" -type f -print -quit 2>/dev/null)" ]] ||
    fail_install "no files under usr/local/share/doc/ilia"
[[ -z "$(find "$stage" \( -name '*.desktop*' -o -iname '*appdata*' -o -name 'gschemas.compiled' \) -print -quit)" ]] ||
    fail_install "desktop/appdata file or compiled schemas staged"
[[ "$(cd "$stage" && find . -mindepth 1 -maxdepth 1)" == "./usr" ]] ||
    fail_install "unexpected top-level staged entries"
while read -r installed; do
    [[ -z "$installed" || "$installed" == \#* ]] && continue
    [[ "$installed" == "$stage" || "$installed" == "$stage"/* ]] || fail_install "install log names $installed outside the stage"
done <"$build/meson-logs/install-log.txt"
for path in /usr/local/bin/ilia /usr/local/share/ilia /usr/local/share/doc/ilia \
    /usr/local/share/glib-2.0/schemas/org.regolith-linux.ilia.gschema.xml \
    /usr/local/share/glib-2.0/schemas/gschemas.compiled; do
    if [[ -e "$path" && "$path" -nt "$tmp/install-marker" ]]; then
        fail_install "$path was written outside the stage"
    fi
done
printf 'ok: meson-install-layout\n'
printf 'ok: all cutover cases passed\n'
