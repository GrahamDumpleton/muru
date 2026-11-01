"""The runner for commands declared with ``call`` (muru._command)."""

from __future__ import annotations

import json
import sys
import textwrap
from pathlib import Path

import pytest

from muru import _command


@pytest.fixture
def ops(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    module = tmp_path / "ops_for_test.py"
    module.write_text(
        textwrap.dedent(
            """
            import os

            calls = []

            def backup(target="s3://default"):
                calls.append((target, os.environ.get("MURU_COMMAND_INPUTS")))
                return 3 if target == "fail" else None

            async def warm(count="1"):
                calls.append(("warm", count))
                return int(count)

            class Jobs:
                @staticmethod
                def reindex():
                    calls.append(("reindex",))
            """
        )
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    yield tmp_path
    sys.modules.pop("ops_for_test", None)


def test_the_callable_gets_the_inputs_as_keywords(
    ops: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MURU_COMMAND_INPUTS", json.dumps({"target": "s3://elsewhere"}))
    assert _command.main(["muru._command", "ops_for_test:backup"]) == 0

    import ops_for_test

    # The inputs left the environment before the call.

    assert ops_for_test.calls == [("s3://elsewhere", None)]


def test_an_integer_is_the_exit_status_and_coroutines_are_awaited(
    ops: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("MURU_COMMAND_INPUTS", json.dumps({"target": "fail"}))
    assert _command.main(["muru._command", "ops_for_test:backup"]) == 3
    monkeypatch.setenv("MURU_COMMAND_INPUTS", json.dumps({"count": "5"}))
    assert _command.main(["muru._command", "ops_for_test:warm"]) == 5
    monkeypatch.delenv("MURU_COMMAND_INPUTS", raising=False)
    assert _command.main(["muru._command", "ops_for_test:Jobs.reindex"]) == 0


def test_a_target_that_is_not_module_function_is_refused(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert _command.main(["muru._command", "ops_for_test"]) == 2
    assert "module:function" in capsys.readouterr().err
