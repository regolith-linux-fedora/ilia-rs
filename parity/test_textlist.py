import pytest
import time

from parity.conftest import ROOT


ITEMS = (ROOT / "parity/fixtures/textlist-items.txt").read_text()


@pytest.mark.parametrize("query,expected", [(None, "Alpha"), ("needle", "Needle Item")])
def test_textlist_selection(isolated_env, run_ilia, query, expected):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "textlist", "-n", stdin=ITEMS)
    if query:
        app.type(query)
    app.key("Return")
    code, stdout, stderr = app.finish()
    assert code == 0
    assert stdout == expected + "\n"
    assert "No page loaded" not in stderr


def test_textlist_unmatched_enter_is_noop(isolated_env, run_ilia):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "textlist", "-n", stdin=ITEMS)
    app.type("unmatched free text")
    app.key("Return")
    time.sleep(0.25)
    assert app.proc.poll() is None
    app.key("Escape")
    code, stdout, _ = app.finish()
    assert code == 0
    assert stdout == ""


def test_textlist_label_and_icon_arguments(isolated_env, run_ilia):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "textlist", "-l", "Fixture", "-i", "applications-other", stdin="Only\n")
    app.key("Return")
    assert app.finish()[:2] == (0, "Only\n")
