"""Unit tests for hook loading and the context API."""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

import muru
from muru._context import Context, _set
from muru.hooks import HOOKS, Hooks


@pytest.fixture
def modules(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.syspath_prepend(str(tmp_path))
    for name in ("hooks_full", "hooks_bad", "hooks_none"):
        sys.modules.pop(name, None)
    return tmp_path


def test_context_defaults_outside_muru(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("muru._context._current", None)
    context = muru.context()
    assert context == Context()
    assert context.worker_id is None
    assert context.process_model == "threaded"


def test_context_is_what_the_host_set(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("muru._context._current", None)
    _set(Context(app="shop", worker_id=3))
    assert muru.context().app == "shop"
    assert muru.context().worker_id == 3


def test_no_module_means_no_hooks() -> None:
    hooks = Hooks.load(None)
    assert not any(hooks.has(name) for name in HOOKS)
    hooks.call("on_load", Context())


def test_hooks_are_collected_and_called(modules: Path) -> None:
    (modules / "hooks_full.py").write_text(
        "CALLS = []\n"
        + "".join(
            f"def {name}(context):\n    CALLS.append(({name!r}, context.app))\n" for name in HOOKS
        )
    )
    hooks = Hooks.load("hooks_full")
    assert all(hooks.has(name) for name in HOOKS)
    hooks.call("post_fork", Context(app="shop"))
    assert sys.modules["hooks_full"].CALLS == [("post_fork", "shop")]


def test_partial_module_is_fine(modules: Path) -> None:
    (modules / "hooks_none.py").write_text("VALUE = 1\n")
    hooks = Hooks.load("hooks_none")
    assert not hooks.has("on_load")


def test_non_callable_hook_is_rejected(modules: Path) -> None:
    (modules / "hooks_bad.py").write_text("post_fork = 42\n")
    with pytest.raises(TypeError, match=r"hooks_bad\.post_fork"):
        Hooks.load("hooks_bad")


def test_missing_module_raises_import_error() -> None:
    with pytest.raises(ImportError):
        Hooks.load("no_such_hooks_module")


def test_call_logged_reports_and_continues(capsys: pytest.CaptureFixture[str]) -> None:
    def on_drain(context: Context) -> None:
        raise RuntimeError("drain-boom")

    hooks = Hooks({"on_drain": on_drain})
    assert hooks.call_logged("on_drain", Context()) is False
    captured = capsys.readouterr().err
    assert "the on_drain hook failed" in captured
    assert "drain-boom" in captured
    assert hooks.call_logged("on_exit", Context()) is True
