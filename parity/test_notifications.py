import json
import time
import threading
from pathlib import Path

import pytest

from parity.conftest import ROOT
from parity.fakes.servers import RoficationServer


SOCKET = Path("/tmp/rofi_notification_daemon")
ROFICATION_LOCK = threading.Lock()


@pytest.fixture
def rofication(tmp_path):
    data = json.loads((ROOT / "parity/fixtures/notifications.json").read_text())
    transcript = tmp_path / "rofication.jsonl"
    with ROFICATION_LOCK:
        with RoficationServer(SOCKET, data, transcript) as server:
            yield server


def _wait_request(server, prefix):
    for _ in range(100):
        matches = [item for item in _transcript_requests(server) if item.startswith(prefix)]
        if matches:
            return matches[-1]
        time.sleep(0.02)
    raise AssertionError(server.requests)


def _transcript_requests(server):
    return [json.loads(line)["request"] for line in server.transcript.read_text().splitlines()]


def _wait_transcript(server, expected):
    for _ in range(100):
        requests = _transcript_requests(server)
        if len(requests) >= len(expected):
            assert requests == expected
            return
        time.sleep(0.02)
    raise AssertionError(server.requests)


def test_delete_selected_notification(isolated_env, run_ilia, rofication):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "notifications")
    _wait_request(rofication, "list")
    app.key("Delete")
    assert _wait_request(rofication, "del:") == "del:11\n"
    _wait_transcript(rofication, ["list\n", "del:11\n", "list\n"])
    app.key("Escape")
    assert app.finish()[0] == 0


def test_filtered_delete_many(isolated_env, run_ilia, rofication):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "notifications")
    _wait_request(rofication, "list")
    app.type("Messenger")
    app.key("shift+Delete")
    request = _wait_request(rofication, "delm:")
    assert "22" in request and "11" not in request
    assert request in _transcript_requests(rofication)
    app.key("Escape")
    assert app.finish()[0] == 0


def test_body_filter_controls_delete_many(isolated_env, run_ilia, rofication):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "notifications")
    _wait_request(rofication, "list")
    app.type("alpha body")
    app.key("shift+Delete")
    request = _wait_request(rofication, "delm:")
    assert "11" in request and "22" not in request
    assert request in _transcript_requests(rofication)
    app.key("Escape")
    assert app.finish()[0] == 0
