"""Install deterministic executable fixtures used by page tests."""

import shutil
from pathlib import Path


def _write_executable(path, body):
    Path(path).write_text("#!/bin/sh\nset -eu\n" + body)
    Path(path).chmod(0o755)


def _install_systemd_runner(bindir, log):
    _write_executable(
        bindir / "systemd-run",
        f'printf "systemd:%s\\n" "$*" >> "{log}"\n'
        'while [ "$#" -gt 0 ]; do case "$1" in --user|--scope) shift;; '
        '--unit) shift 2;; *) break;; esac; done\n'
        'exec "$@"\n',
    )


def install_app_fakes(env, bindir, tmp_path, fixture_dir):
    apps = Path(env["XDG_DATA_HOME"]) / "applications"
    shutil.copytree(fixture_dir, apps)
    log = tmp_path / "apps.log"
    _write_executable(
        bindir / "fixture-app",
        f'printf "app:%s\\n" "$*" >> "{log}"\n',
    )
    _install_systemd_runner(bindir, log)
    return log


def install_terminal_fakes(bindir, tmp_path):
    log = tmp_path / "commands.log"
    for name, prefix in (
        ("fixture-command", "fixture"),
        ("free-command", "free"),
        ("x-terminal-emulator", "terminal"),
    ):
        _write_executable(
            bindir / name,
            f'printf "{prefix}:%s\\n" "$*" >> "{log}"\n',
        )
    _install_systemd_runner(bindir, log)
    return log
