import time

from parity.fakes.executables import install_terminal_fakes


def _wait_log(log, needle):
    for _ in range(100):
        text = log.read_text() if log.exists() else ""
        if needle in text:
            return text
        time.sleep(0.02)
    raise AssertionError(f"missing {needle!r} in {text!r}")


def test_quiet_command_and_systemd_wrapper(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_terminal_fakes(bindir, tmp_path)
    app = run_ilia(env, "-p", "terminal", "-q")
    app.type("fixture-command")
    app.key("Return")
    assert app.finish()[0] == 0
    text = _wait_log(log, "fixture:")
    lines = text.splitlines()
    assert len(lines) == 2
    assert lines[0].startswith("systemd:--user --scope --unit run_ilia_")
    assert lines[0].endswith(f" {bindir}/fixture-command")
    assert lines[1] == "fixture:"


def test_terminal_wrapper(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_terminal_fakes(bindir, tmp_path)
    app = run_ilia(env, "-p", "terminal")
    app.type("fixture-command")
    app.key("Return")
    assert app.finish()[0] == 0
    text = _wait_log(log, "terminal:")
    lines = text.splitlines()
    assert len(lines) == 2
    assert lines[0].startswith("systemd:--user --scope --unit run_ilia_")
    assert lines[0].endswith(
        f" x-terminal-emulator -e {bindir}/fixture-command"
    )
    assert lines[1] == f"terminal:-e {bindir}/fixture-command"


def test_terminal_unmatched_enter_is_noop(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_terminal_fakes(bindir, tmp_path)
    app = run_ilia(env, "-p", "terminal", "-q")
    app.type("free-command hello")
    app.key("Return")
    time.sleep(0.2)
    assert app.proc.poll() is None
    assert not log.exists()
    app.key("Escape")
    code, stdout, _ = app.finish()
    assert code == 0
    assert stdout == ""
    assert not log.exists()
