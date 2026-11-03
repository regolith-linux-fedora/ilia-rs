import time


def test_no_miner_is_graceful(isolated_env, run_ilia):
    env, _ = isolated_env
    app = run_ilia(env, "-p", "tracker")
    app.type("fixture query")
    time.sleep(0.3)
    assert app.proc.poll() is None
    app.key("Escape")
    code, _, stderr = app.finish()
    assert code == 0
    assert "org.freedesktop.Tracker3.Miner.Files" in stderr
