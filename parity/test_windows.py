import json
import re
import time

from parity.conftest import ROOT
from parity.fakes.servers import I3Server


def _tree():
    return json.loads((ROOT / "parity/fixtures/window-tree.json").read_text())


def _wait_request(server):
    for _ in range(100):
        if server.transcript.exists() and server.transcript.stat().st_size:
            return
        time.sleep(0.02)
    raise AssertionError("Ilia did not request window tree")


def _finish_after_activation(app):
    time.sleep(0.2)
    if app.proc.poll() is None:
        app.key("Escape")
    return app.finish()


def _assert_tree_request(server, transcript):
    records = [json.loads(line) for line in transcript.read_text().splitlines()]
    assert records[0] == {
        "message_type": 4, "payload_hex": "",
    }
    return records


def _assert_focus_or_absent(records, stderr, con_id):
    request = {
        "message_type": 0,
        "payload_hex": f'[con_id="{con_id}"] focus'.encode().hex(),
    }
    if request in records:
        return
    assert re.search(
        r"Error: execute_keybinding failed: .*/usr/bin/i3-msg.*"
        r"(?:No such file or directory|not found in \$PATH)",
        stderr,
    ), stderr


def test_window_selection_uses_expected_con_id(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    with I3Server(socket_path, {4: _tree()}, transcript) as server:
        app = run_ilia(env, "-p", "windows")
        _wait_request(server)
        app.type("Editor Fixture")
        app.key("Return")
        code, _, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_tree_request(server, transcript)
    _assert_focus_or_absent(records, stderr, 101)


def test_i3bar_node_is_not_selectable(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    with I3Server(socket_path, {4: _tree()}, transcript) as server:
        app = run_ilia(env, "-p", "windows")
        _wait_request(server)
        app.type("Forbidden Bar")
        app.key("Return")
        app.key("Escape")
        code, _, stderr = app.finish()
    assert code == 0
    records = _assert_tree_request(server, transcript)
    assert all(record["message_type"] != 0 for record in records)
    assert "i3-msg" not in stderr and "execute_keybinding" not in stderr


def test_floating_window_is_navigable(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    with I3Server(socket_path, {4: _tree()}, transcript) as server:
        app = run_ilia(env, "-p", "windows")
        _wait_request(server)
        app.type("Floating Fixture")
        app.key("Return")
        code, _, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_tree_request(server, transcript)
    _assert_focus_or_absent(records, stderr, 303)
