import re
import time
from pathlib import Path

from parity.conftest import ROOT
from parity.fakes.executables import install_app_fakes


def _read(log, needle):
    for _ in range(100):
        text = log.read_text() if log.exists() else ""
        if needle in text:
            return text
        time.sleep(0.02)
    raise AssertionError(text)


def test_app_keyword_filter_launches_visible_entry(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    app = run_ilia(env, "-p", "apps")
    app.type("needle")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    text = _read(log, "app:alpha")
    lines = text.splitlines()
    assert len(lines) == 2
    assert re.fullmatch(
        r"systemd:--user --scope --unit "
        r"run_ilia_alpha\.desktop_[0-9a-f]{8}\.scope fixture-app alpha",
        lines[0],
    )
    assert lines[1] == "app:alpha"


def test_app_launch_fills_field_codes_in_the_entry_path(isolated_env, run_ilia, tmp_path):
    # Upstream #168: %c is the name, %i the icon, %k the desktop file, each
    # one literal argument; file and URL codes leave nothing; the program
    # starts in the entry's Path, inside the scope.
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    workdir = tmp_path / "work dir"
    workdir.mkdir()
    probe = bindir / "fields-probe"
    probe.write_text(
        "#!/bin/sh\nset -eu\n"
        f'printf "cwd:%s\\n" "$(pwd -P)" >> "{log}"\n'
        f'for arg in "$@"; do printf "arg:%s\\n" "$arg" >> "{log}"; done\n'
        f'printf "done\\n" >> "{log}"\n'
    )
    probe.chmod(0o755)
    entry = Path(env["XDG_DATA_HOME"]) / "applications" / "fields.desktop"
    entry.write_text(
        "[Desktop Entry]\nType=Application\nName=Fields $HOME Fixture\n"
        "Keywords=fieldcodes;\nIcon=fixture-icon\n"
        f"Path={workdir}\n"
        "Exec=fields-probe %c --class=%c %i %k 100%% %f %U\n"
    )
    app = run_ilia(env, "-p", "apps")
    app.type("fieldcodes")
    app.key("Return")
    assert app.finish(timeout=8)[0] == 0
    lines = _read(log, "done").splitlines()
    assert re.fullmatch(
        r"systemd:--user --scope --unit run_ilia_fields\.desktop_[0-9a-f]{8}\.scope "
        r"fields-probe .*",
        lines[0],
    )
    assert lines[1:] == [
        f"cwd:{workdir.resolve()}",
        "arg:Fields $HOME Fixture",
        "arg:--class=Fields $HOME Fixture",
        "arg:--icon",
        "arg:fixture-icon",
        f"arg:{entry}",
        "arg:100%",
        "done",
    ]


def test_hidden_app_not_selectable_and_escape_launches_nothing(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    app = run_ilia(env, "-p", "apps")
    app.type("Hidden Fixture")
    app.key("Return")
    app.key("Escape")
    assert app.finish()[0] == 0
    assert not log.exists()


def test_escape_without_activation_launches_nothing(isolated_env, run_ilia, tmp_path):
    env, bindir = isolated_env
    log = install_app_fakes(env, bindir, tmp_path, ROOT / "parity/fixtures/apps")
    app = run_ilia(env, "-p", "apps")
    app.key("Escape")
    assert app.finish()[0] == 0
    assert not log.exists()
