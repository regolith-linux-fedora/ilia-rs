"""The Calculator page (ported ahead of upstream #172).

The cases need the page's helper, `ilia-calculator`, beside the binary (as
meson builds it in `build/src`) or installed, and xclip to read the
clipboard and to take it over.
"""

import select
import subprocess
import time
from pathlib import Path

import pytest

from parity.conftest import BINARY, _wait_for


HELPER = next(
    (
        path
        for path in (BINARY.parent / "ilia-calculator", Path("/usr/libexec/ilia-calculator"))
        if path.is_file()
    ),
    None,
)
XCLIP = Path("/usr/bin/xclip")

pytestmark = [
    pytest.mark.skipif(HELPER is None, reason="no ilia-calculator beside the binary or installed"),
    pytest.mark.skipif(not XCLIP.exists(), reason="xclip is not installed"),
]


def _has_locale(name):
    result = subprocess.run(["locale", "-a"], text=True, capture_output=True)
    wanted = name.lower().replace("-", "")
    return any(line.lower().replace("-", "") == wanted for line in result.stdout.splitlines())


def _read_line(app, timeout=5):
    ready, _, _ = select.select([app.proc.stdout], [], [], timeout)
    return app.proc.stdout.readline() if ready else None


def _clipboard(env):
    result = subprocess.run(
        [str(XCLIP), "-selection", "clipboard", "-o"],
        env=env, text=True, capture_output=True, timeout=5,
    )
    return result.stdout if result.returncode == 0 else None


def _hand_over_clipboard(app, env):
    """Another program copies: the launcher holding the answer exits."""
    taker = subprocess.Popen(
        [str(XCLIP), "-selection", "clipboard", "-i", "-quiet", "-loops", "1"],
        env=env, stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    taker.stdin.write(b"copied elsewhere")
    taker.stdin.close()
    try:
        code, stdout, stderr = app.finish(timeout=5)
        # Reading it once ends the taker.
        assert _clipboard(env) == "copied elsewhere"
    finally:
        try:
            taker.wait(timeout=5)
        except subprocess.TimeoutExpired:
            taker.kill()
            taker.wait()
    return code, stdout, stderr


@pytest.fixture
def calculator_env(isolated_env):
    env, bindir = isolated_env
    for name in list(env):
        if name.startswith("LC_") or name in ("LANG", "LANGUAGE"):
            del env[name]
    env["LC_ALL"] = "C.UTF-8"
    return env, bindir


@pytest.mark.parametrize(
    "text,shown",
    [
        ("2^64", "18446744073709551616"),
        ("1/7", "0.142857142857143"),
        ("-3^2", "-9"),
        ("2^3^2", "512"),
        ("3!^0", "1"),
        ("0*5!", "0"),
    ],
)
def test_enter_prints_the_answer_and_holds_it_until_the_clipboard_changes_hands(
    calculator_env, run_ilia, text, shown,
):
    env, _ = calculator_env
    app = run_ilia(env, "-p", "calculator")
    app.type(text)
    # Enter right away waits for the answer.
    app.key("Return")
    assert _read_line(app) == shown + "\n"
    # No clipboard manager runs: the launcher serves the answer without a
    # window.
    _wait_for(lambda: _clipboard(env) == shown, message="the answer on the clipboard")
    time.sleep(0.5)
    assert app.proc.poll() is None
    code, stdout, _ = _hand_over_clipboard(app, env)
    assert (code, stdout) == (0, "")


@pytest.mark.parametrize(
    "text",
    [
        # A part with no value leaves the whole expression without one, even
        # where the rest of it would cancel that part.
        "0*(1.5!)",
        "(-3)!^0",
        "(1.5!)-(1.5!)",
        "0*(-1)!",
        "0*tan(pi/2)",
        "5/0",
        "sqrt(-4)",
        "3*",
        "3 kg",
    ],
)
def test_enter_without_an_answer_does_nothing(calculator_env, run_ilia, text):
    env, _ = calculator_env
    app = run_ilia(env, "-p", "calculator")
    app.type(text)
    app.key("Return")
    time.sleep(2)
    assert app.proc.poll() is None, "the window closed"
    assert _clipboard(env) is None
    app.key("Escape")
    code, stdout, _ = app.finish()
    assert (code, stdout) == (0, "")


@pytest.mark.skipif(not _has_locale("de_DE.UTF-8"), reason="no de_DE.UTF-8 locale")
@pytest.mark.parametrize(
    "variables,shown",
    [
        ({"LC_ALL": "de_DE.UTF-8"}, "0,125"),
        ({"LC_NUMERIC": "de_DE.UTF-8", "LANG": "C.UTF-8"}, "0,125"),
        ({"LC_ALL": "C.UTF-8", "LC_NUMERIC": "de_DE.UTF-8"}, "0.125"),
        ({"LC_MESSAGES": "de_DE.UTF-8", "LANG": "C.UTF-8"}, "0.125"),
    ],
)
def test_the_decimal_mark_is_the_locale_one(calculator_env, run_ilia, variables, shown):
    env, _ = calculator_env
    env.pop("LC_ALL")
    env.update(variables)
    app = run_ilia(env, "-p", "calculator")
    # Either mark is read in any locale.
    app.type("1,5/12" if "LC_ALL" in variables else "1.5/12")
    app.key("Return")
    assert _read_line(app) == shown + "\n"
    code, stdout, _ = _hand_over_clipboard(app, env)
    assert (code, stdout) == (0, "")
