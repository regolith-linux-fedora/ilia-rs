import json
import re
import shutil
import time

from parity.conftest import ROOT
from parity.fakes.servers import I3Server

I3_CONFIG = json.loads((ROOT / "parity/fixtures/i3-config.json").read_text())


def _wait_request(server):
    for _ in range(100):
        if server.transcript.exists() and server.transcript.stat().st_size:
            return
        time.sleep(0.02)
    raise AssertionError("Ilia did not request WM config")


def _finish_after_activation(app):
    time.sleep(0.2)
    if app.proc.poll() is None:
        app.key("Escape")
    return app.finish()


def _assert_config_request(server, transcript):
    records = [json.loads(line) for line in transcript.read_text().splitlines()]
    assert records[0] == {
        "message_type": 9, "payload_hex": "",
    }
    return records


def _assert_wm_activation(records, stderr, binary, command):
    command_record = {"message_type": 0, "payload_hex": command.encode().hex()}
    if command_record in records:
        return
    pattern = (
        rf"Error: execute_keybinding failed: .*{re.escape(binary)}.*"
        rf"(?:No such file or directory|not found in \$PATH)"
    )
    assert re.search(pattern, stderr), stderr


def test_i3_config_is_loaded_and_binding_selected(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    with I3Server(socket_path, {9: I3_CONFIG}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Open fixture terminal")
        app.key("Return")
        code, _, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stderr, "/usr/bin/i3-msg", "exec fixture-terminal ")


def test_sway_config_and_includes_are_walked(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "sway.sock"
    transcript = tmp_path / "sway-ipc.jsonl"
    includes = tmp_path / "partials"
    shutil.copytree(ROOT / "parity/fixtures/sway-partials", includes)
    env.update({"I3SOCK": str(socket_path), "SWAYSOCK": str(socket_path), "FIXTURE_CONFIG": str(includes)})
    reply = json.loads((ROOT / "parity/fixtures/sway-config.json").read_text())
    with I3Server(socket_path, {9: reply}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Sway fixture action")
        app.key("Return")
        code, _, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stderr, "/usr/bin/swaymsg", "exec fixture-sway ")


def test_second_i3_category_is_parsed(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    with I3Server(socket_path, {9: I3_CONFIG}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Lock fixture")
        app.key("Return")
        code, _, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stderr, "/usr/bin/i3-msg", "exec fixture-lock ")
