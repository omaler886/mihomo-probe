"""Make the offline test suite hermetic.

`config.DATA` resolves to `$MIHOMO_TEST_ROOT/data`, and on the host that path
IS the running service's state directory -- the suite is normally executed
inside the app container, where MIHOMO_TEST_ROOT=/srv/mihomo-test. Tests that
forget to patch one of the module-level paths therefore write into production:
measured before this guard, `test_alerts_lanes` left a `round.state.json` there,
`test_hardening` left a `config.json` (rewritten by `POST /api/config`), and
`test_logic` left a `state.db`. Worse, the leftover `data/config.json` made
`test_live`'s `LIVE_DEPLOYED` check pass, so a plain `unittest discover` started
running the live suite against whatever hostname that file named.

Import this before `mihomo_test` in every offline test module, then call
`isolate()` from `setUpModule()`. `tests/test_live.py` deliberately does not
use it: it exists to talk to a real deployment.
"""
import shutil
import tempfile
from pathlib import Path

_state = {"root": None, "original": None}


def _paths():
    from mihomo_test import config, db, engine, notifier

    return config, db, engine, notifier


def _snapshot(config, db, engine, notifier):
    return {
        "data": config.DATA,
        "config_path": config.CONFIG_PATH,
        "db_path": db.DB_PATH,
        "db_conn": db._conn,
        "round_state": engine.ROUND_STATE,
        "export_dir": engine.EXPORT_DIR,
        "notify_state": notifier.STATE_PATH,
    }


def _apply(config, db, engine, notifier, root):
    if db._conn is not None:
        db._conn.close()
    config.DATA = root
    config.CONFIG_PATH = root / "config.json"
    db.DB_PATH, db._conn = root / "state.db", None
    engine.ROUND_STATE = root / "round.state.json"
    engine.EXPORT_DIR = root / "exports"
    notifier.STATE_PATH = root / "alert-state.json"


def isolate():
    """Point every module-level data path at a throwaway directory."""
    config, db, engine, notifier = _paths()
    _state["original"] = _snapshot(config, db, engine, notifier)
    _state["root"] = Path(tempfile.mkdtemp(prefix="mihomo-test-suite-"))
    _apply(config, db, engine, notifier, _state["root"])
    return _state["root"]


def restore():
    """Put the real paths back, so a later module sees the deployment again."""
    if not _state["root"]:
        return
    config, db, engine, notifier = _paths()
    original = _state["original"]
    if db._conn is not None:
        db._conn.close()
    config.DATA = original["data"]
    config.CONFIG_PATH = original["config_path"]
    db.DB_PATH, db._conn = original["db_path"], original["db_conn"]
    engine.ROUND_STATE = original["round_state"]
    engine.EXPORT_DIR = original["export_dir"]
    notifier.STATE_PATH = original["notify_state"]
    shutil.rmtree(_state["root"], ignore_errors=True)
    _state["root"] = None
