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
    # Leave out the binding-state queries and event subscriptions with which
    # ilia follows the window manager's own bindings (upstream #171).
    requests = [record for record in records if record["message_type"] not in (2, 12)]
    assert requests[0] == {
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


# One i3 file: the terminal key retuned by a resource (`y`), the other row
# at its default (`j`). i3 4.20 and later send each file as written next to
# the text it bound.
I3_RETUNED_RAW = (
    "set $mod Mod4\n"
    "## Launch // Retuned fixture // <Super> Enter ##\n"
    "set_from_resource $key.a wm.binding.a Return\n"
    "bindsym $mod+$key.a exec fixture-retuned\n"
    "\n"
    "## Launch // Default fixture // <Super> Kbdfixture ##\n"
    "set_from_resource $key.b wm.binding.b j\n"
    "bindsym $mod+$key.b exec fixture-default\n"
)
I3_RETUNED_REPLACED = (
    "set Mod4 Mod4\n"
    "## Launch // Retuned fixture // <Super> Enter ##\n"
    "set_from_resource y wm.binding.a Return\n"
    "bindsym Mod4+y exec fixture-retuned\n"
    "\n"
    "## Launch // Default fixture // <Super> Kbdfixture ##\n"
    "set_from_resource j wm.binding.b j\n"
    "bindsym Mod4+j exec fixture-default\n"
)


def _choose_i3_row(env, run_ilia, tmp_path, text):
    socket_path = tmp_path / "i3.sock"
    transcript = tmp_path / "i3-ipc.jsonl"
    env["I3SOCK"] = str(socket_path)
    config = {"included_configs": [{
        "raw_contents": I3_RETUNED_RAW,
        "variable_replaced_contents": I3_RETUNED_REPLACED,
    }]}
    with I3Server(socket_path, {9: config}, transcript) as server:
        app = run_ilia(env, "-p", "keybindings")
        _wait_request(server)
        app.type(text)
        app.key("Return")
        code, stdout, stderr = _finish_after_activation(app)
    assert code == 0
    return _assert_config_request(server, transcript), stdout, stderr


def test_i3_row_shows_and_filters_on_the_key_a_resource_bound(isolated_env, run_ilia, tmp_path):
    # The retuned row's key text is "<Super> y" now, not its comment's
    # "<Super> Enter": the filter finds it by the key it is bound to.
    env, _ = isolated_env
    records, stdout, stderr = _choose_i3_row(env, run_ilia, tmp_path, "fixture y")
    _assert_wm_activation(records, stdout, stderr, "exec fixture-retuned")


def test_i3_row_at_its_default_keeps_its_comment_text(isolated_env, run_ilia, tmp_path):
    # The other row binds what it binds with no resource set, so it keeps
    # its comment's key text, which the filter matches.
    env, _ = isolated_env
    records, stdout, stderr = _choose_i3_row(env, run_ilia, tmp_path, "Kbdfixture")
    _assert_wm_activation(records, stdout, stderr, "exec fixture-default")


def test_sway_mode_and_key_code_rows_run_their_bindings(isolated_env, run_ilia, tmp_path):
    # A comment before a mode block takes the first binding after the block
    # (Regolith's "Enter Resize Mode"), and a row bound only by key code
    # takes its `bindcode` line. Without the resource service every
    # `set_from_resource` takes its default.
    env, _ = isolated_env
    config_dir = tmp_path / "sway"
    config_dir.mkdir()
    main = (
        "set $mod Mod4\n"
        "## Resize // Enter fixture mode // <Super> r ##\n"
        "set_from_resource $small wm.resize.small 6\n"
        'mode "Fixture Mode" {\n'
        "    ## Resize // Leave fixture mode // Escape ##\n"
        '    bindsym Escape mode "default"\n'
        "}\n"
        "set_from_resource $key.r wm.binding.resize_mode r\n"
        'bindsym $mod+$key.r mode "Fixture Mode"\n'
        "\n"
        "## Launch // Code fixture // <Super> z ##\n"
        "bindcode $mod+52 exec fixture-code\n"
    )
    (config_dir / "config").write_text(main)
    rows = [
        ("Enter fixture mode", 'mode "Fixture Mode"'),
        ("Code fixture", "exec fixture-code"),
    ]
    for index, (text, command) in enumerate(rows):
        socket_path = tmp_path / f"sway-{index}.sock"
        transcript = tmp_path / f"sway-ipc-{index}.jsonl"
        env["SWAYSOCK"] = str(socket_path)
        version = _sway_version(config_dir / "config")
        with I3Server(socket_path, {9: {"config": main}, 7: version}, transcript) as server:
            app = run_ilia(env, "-p", "keybindings")
            _wait_request(server)
            app.type(text)
            app.key("Return")
            code, stdout, stderr = _finish_after_activation(app)
        assert code == 0
        records = _assert_config_request(server, transcript)
        _assert_wm_activation(records, stdout, stderr, command)
