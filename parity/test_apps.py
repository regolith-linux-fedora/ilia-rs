import re
import time

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
