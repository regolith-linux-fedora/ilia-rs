import subprocess

from parity.conftest import BINARY


def test_help_contract():
    result = subprocess.run([str(BINARY), "-h"], text=True, capture_output=True, timeout=5)
    expected = (
        "Usage: ilia [-t stylesheet] [-n] [-a] [-p page]\n"
        "\n"
        "\t-t: specify path to custom stylesheet.\n"
        "\n"
        "\t-n: no custom styles\n"
        "\n"
        "\t-a: load all pages\n"
        "\n"
        "pages:\n"
        "\t'apps' - launch desktop applications (default)\n"
        "\t'terminal' - launch a terminal command\n"
        "\t\t-q: quiet execution\n"
        "\t'notifications' - launch notifications manager\n"
        "\t'keybindings' - launch keybindings viewer\n"
        "\t'textlist' - select an item from a specified list\n"
        "\t\t-l: page label\n"
        "\t\t-i: page/item icon\n"
        "\t\t-n: no icon\n"
        "\t'windows' - navigate to a window\n"
        "\t'tracker' - search for files by content\n"
    )
    assert (result.returncode, result.stdout, result.stderr) == (0, expected, "")


def test_version_contract():
    result = subprocess.run([str(BINARY), "-v"], text=True, capture_output=True, timeout=5)
    assert (result.returncode, result.stdout, result.stderr) == (0, "ilia version 0.19\n", "")


def test_unknown_page_contract(isolated_env):
    env, _ = isolated_env
    result = subprocess.run(
        [str(BINARY), "-p", "not-a-page"], env=env, text=True,
        capture_output=True, timeout=5,
    )
    assert result.returncode == 1
    assert result.stdout == ""
    assert result.stderr == "Unknown page type: not-a-page\nNo page loaded, exiting\n"
