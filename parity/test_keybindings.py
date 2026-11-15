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


def _assert_wm_activation(records, stdout, stderr, command):
    # The chosen binding reaches the window manager once, as one RUN_COMMAND
    # request on the same socket, with its text unchanged. Nothing is
    # printed.
    runs = [record for record in records if record["message_type"] == 0]
    assert runs == [{"message_type": 0, "payload_hex": command.encode().hex()}], records
    assert stdout == ""
    assert "execute_keybinding" not in stderr, stderr


def _sway_version(config_path):
    # sway sends the version parts as numbers.
    return {
        "human_readable": "1.11", "loaded_config_file_name": str(config_path),
        "major": 1, "minor": 11, "patch": 0,
    }


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
        code, stdout, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stdout, stderr, "exec fixture-terminal")


def test_sway_config_and_includes_are_walked(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "sway.sock"
    transcript = tmp_path / "sway-ipc.jsonl"
    includes = tmp_path / "partials"
    shutil.copytree(ROOT / "parity/fixtures/sway-partials", includes)
    # sway is reached through SWAYSOCK; an I3SOCK left from another session
    # must not be used instead.
    env.update({
        "I3SOCK": str(tmp_path / "stale-i3.sock"),
        "SWAYSOCK": str(socket_path),
        "FIXTURE_CONFIG": str(includes),
    })
    reply = json.loads((ROOT / "parity/fixtures/sway-config.json").read_text())
    version = _sway_version(tmp_path / "sway-config")
    with I3Server(socket_path, {9: reply, 7: version}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Sway fixture action")
        app.key("Return")
        code, stdout, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stdout, stderr, "exec fixture-sway")


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
        code, stdout, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stdout, stderr, "exec fixture-lock")


def test_quoted_command_is_sent_as_is_and_a_failure_reported(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    command = '[class="Fixture  App"] exec notify-send "Caf\u00e9  \\"x\\""'
    config = {"included_configs": [{
        "variable_replaced_contents":
            "## Launch // Quoted fixture // <Super>q ##\n"
            f"bindsym --release $mod+q {command}\n",
    }]}
    failure = [{"success": False, "error": "Fixture failure"}]
    with I3Server(socket_path, {9: config, 0: failure}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Quoted fixture")
        app.key("Return")
        # ilia closes even though the window manager reports a failure.
        code, stdout, stderr = app.finish()
    assert code == 0
    records = _assert_config_request(server, transcript)
    runs = [record for record in records if record["message_type"] == 0]
    assert runs == [{"message_type": 0, "payload_hex": command.encode().hex()}], records
    assert stdout == ""
    assert f"Error: execute_keybinding failed: {command}: Fixture failure" in stderr, stderr


def test_sway_relative_include_is_read_beside_the_main_file(isolated_env, run_ilia, tmp_path):
    env, _ = isolated_env
    socket_path = tmp_path / "sway.sock"
    transcript = tmp_path / "sway-ipc.jsonl"
    config_dir = tmp_path / "sway"
    (config_dir / "config.d").mkdir(parents=True)
    main = "set $cmd fixture-main\ninclude config.d/*\n"
    (config_dir / "config").write_text(main)
    # The first file has no final newline, and the second one's row must
    # still be found; `$cmd` has the value it holds where each binding is.
    (config_dir / "config.d" / "20-second").write_text(
        "set $cmd fixture-second\n## Included // Second fixture // <Super>2 ##\nbindsym $mod+2 exec $cmd"
    )
    (config_dir / "config.d" / "30-third").write_text(
        "## Included // Third fixture // <Super>3 ##\nbindsym $mod+3 exec $cmd  third\n"
    )
    env["SWAYSOCK"] = str(socket_path)
    version = _sway_version(config_dir / "config")
    with I3Server(socket_path, {9: {"config": main}, 7: version}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type("Third fixture")
        app.key("Return")
        code, stdout, stderr = _finish_after_activation(app)
    assert code == 0
    records = _assert_config_request(server, transcript)
    _assert_wm_activation(records, stdout, stderr, "exec fixture-second third")
