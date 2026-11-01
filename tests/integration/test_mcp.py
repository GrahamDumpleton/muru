"""The MCP server for coding agents (design specification section 10.11).

`muructl mcp serve` speaks the Model Context Protocol on standard input and
output: an agent lists its tools, and each tool runs the matching muructl
command with --json against the same daemon. --read-only and --allow limit
the tools that change things, on the server's side.
"""

from __future__ import annotations

import json
import subprocess
import sys
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from support import server as support
from support.daemon import SHOP, Daemon
from support.daemon import daemon as daemon_fixture

daemon = daemon_fixture


class Session:
    """An MCP client talking to one `muructl mcp serve` over its pipes."""

    def __init__(self, process: subprocess.Popen[str]) -> None:
        self.process = process
        self.next_id = 0

    def request(self, method: str, params: dict[str, Any] | None = None) -> dict[str, Any]:
        """Send a request and return its result, failing on a protocol error."""

        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}})

        # Notifications may arrive first; the response carries the id.

        while True:
            line = self.process.stdout.readline() if self.process.stdout else ""
            assert line, "the server closed its output"
            message = json.loads(line)

            if message.get("id") == self.next_id:
                assert "error" not in message, message
                result: dict[str, Any] = message["result"]
                return result

    def send(self, message: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def tools(self) -> dict[str, dict[str, Any]]:
        return {tool["name"]: tool for tool in self.request("tools/list")["tools"]}

    def call(self, tool: str, /, **arguments: Any) -> tuple[bool, str]:
        """Call a tool: whether it failed, and the text it returned."""

        result = self.request("tools/call", {"name": tool, "arguments": arguments})
        return bool(result.get("isError")), result["content"][0]["text"]


@contextmanager
def session(
    daemon: Daemon, *options: str, cwd: Path | None = None
) -> Iterator[tuple[Session, dict[str, Any]]]:
    """Start `muructl mcp serve` against the daemon, from `cwd` (the
    project it serves), and initialize it."""

    muructl = support.muructl_path()
    assert muructl is not None
    process = subprocess.Popen(
        [str(muructl), "mcp", "serve", "--socket", str(daemon.run_dir / "admin.sock"), *options],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
        cwd=cwd,
    )
    client = Session(process)

    try:
        initialized = client.request(
            "initialize",
            {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"},
            },
        )
        client.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        yield client, initialized
    finally:
        if process.stdin:
            process.stdin.close()
        process.wait(timeout=10)


def test_tools_run_the_commands_they_name(daemon: Daemon) -> None:
    with session(daemon) as (client, initialized):
        assert initialized["serverInfo"]["name"] == "muru"
        assert "app@release" in initialized["instructions"]

        tools = client.tools()
        assert {"status", "list", "release", "deploy", "rollback", "docs"} <= set(tools)
        assert tools["status"]["annotations"]["readOnlyHint"] is True
        assert tools["deploy"]["annotations"]["readOnlyHint"] is False
        assert "debug" in tools and "exec" not in json.dumps(tools["debug"]["inputSchema"])

        failed, text = client.call("deploy", app="shop", source=str(SHOP), python=sys.executable)
        assert not failed, text
        release = json.loads(text)
        assert (release["id"], release["state"], release["serving"]) == ("r1", "ready", True)

        failed, text = client.call("list", what="releases", app="shop")
        assert [entry["id"] for entry in json.loads(text)["releases"]] == ["r1"]

        failed, text = client.call("logs", target="shop", lines=5)
        assert not failed and isinstance(json.loads(text), list)


def test_a_failing_command_is_a_tool_error_with_its_sentence(daemon: Daemon) -> None:
    with session(daemon) as (client, _):
        failed, text = client.call("release", release="shop@r9")
        assert failed
        assert "does not exist" in text

        failed, text = client.call("release")
        assert failed and "release is required" in text


def test_read_only_offers_no_tool_that_changes_anything(daemon: Daemon) -> None:
    with session(daemon, "--read-only") as (client, initialized):
        tools = client.tools()
        assert "status" in tools and "deploy" not in tools
        assert "read-only" in initialized["instructions"]
        assert all(tool["annotations"]["readOnlyHint"] for tool in tools.values())


def test_allow_limits_the_tools_that_change_things(daemon: Daemon) -> None:
    with session(daemon, "--allow", "deploy,drain") as (client, _):
        tools = client.tools()
        assert {"deploy", "drain", "status"} <= set(tools)
        assert "rollback" not in tools and "apply" not in tools


def test_the_docs_tool_returns_the_user_guide(daemon: Daemon) -> None:
    with session(daemon) as (client, _):
        failed, text = client.call("docs")
        assert not failed and "reference/json-output" in text.splitlines()

        failed, text = client.call("docs", page="json-output")
        assert not failed and text.startswith("# JSON output")


def skill(project: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Run a `muructl mcp skill` command in a project directory."""

    muructl = support.muructl_path()
    assert muructl is not None
    return subprocess.run(
        [str(muructl), "mcp", "skill", *arguments], cwd=project, capture_output=True, text=True
    )


def test_the_skill_installs_into_the_project_and_reports_its_state(tmp_path: Path) -> None:
    missing = skill(tmp_path, "status", "--json")
    assert missing.returncode == 1
    assert json.loads(missing.stdout)["state"] == "missing"

    installed = skill(tmp_path, "install")
    assert installed.returncode == 0, installed.stderr
    path = tmp_path / ".claude" / "skills" / "muru" / "SKILL.md"
    assert path.read_text() == skill(tmp_path, "print").stdout
    assert path.read_text().startswith("---\nname: muru\n")

    current = skill(tmp_path, "status")
    assert current.returncode == 0 and "matches" in current.stdout

    # An older or edited copy is out of date, and install puts it right.

    path.write_text(path.read_text().replace("Developing with Muru", "Old"))
    assert json.loads(skill(tmp_path, "status", "--json").stdout)["state"] == "out_of_date"
    skill(tmp_path, "install")
    assert skill(tmp_path, "status").returncode == 0

    other = skill(tmp_path, "install", "--dir", "agents")
    assert other.returncode == 0 and (tmp_path / "agents" / "muru" / "SKILL.md").exists()


def test_the_server_says_when_the_project_s_skill_is_missing(
    daemon: Daemon, tmp_path: Path
) -> None:
    with session(daemon, cwd=tmp_path) as (client, initialized):
        assert "muructl mcp skill install" in initialized["instructions"]
        failed, text = client.call("skill_status")
        assert not failed and json.loads(text)["state"] == "missing"

    skill(tmp_path, "install")

    with session(daemon, cwd=tmp_path) as (client, initialized):
        assert "muructl mcp skill install" not in initialized["instructions"]
        assert json.loads(client.call("skill_status")[1])["state"] == "current"


COMMANDS_PROJECT = """
[project]
name = "ops"
version = "0.1.0"

[tool.muru]
entrypoint = "app:application"
processes = 1

[tool.muru.commands.greet]
run = ["python", "-c", "import os, sys; print('hello', sys.argv[1], os.environ['WORD'])", "{who}"]
env = { WORD = "{word}" }
inputs.who = { required = true }
inputs.word = { secret = true }

[tool.muru.commands.fail]
run = ["python", "-c", "import sys; print('failing', file=sys.stderr); sys.exit(3)"]
"""


def test_declared_commands_run_through_the_tools(daemon: Daemon, tmp_path: Path) -> None:
    project = tmp_path / "ops"
    project.mkdir()
    (project / "pyproject.toml").write_text(COMMANDS_PROJECT)
    (project / "app.py").write_text(
        "def application(environ, start_response):\n"
        "    start_response('200 OK', [])\n"
        "    return [b'ok']\n"
    )
    assert daemon.deploy("ops", project) == "muru: release ops@r1 is ready and serving"

    with session(daemon) as (client, initialized):
        assert "run_command" in initialized["instructions"]

        failed, text = client.call("commands", target="ops")
        assert not failed, text
        names = [command["name"] for command in json.loads(text)["commands"]]
        assert names == ["fail", "greet"]

        failed, text = client.call("run_command", target="ops", name="greet", inputs={"who": "bob"})
        assert not failed, text
        ran = json.loads(text)
        assert (ran["output"], ran["exit_code"], ran["release"]) == (["hello bob "], 0, "r1")

        # A secret input given as a value is refused before anything runs.

        failed, text = client.call(
            "run_command", target="ops", name="greet", inputs={"who": "bob", "word": "x"}
        )
        assert failed and "the input word is secret" in text, text

        failed, text = client.call("run_command", target="ops", name="fail")
        assert failed, text
        ran = json.loads(text)
        assert ran["output"] == ["stderr: failing"] and ran["exit_code"] == 3, ran

        failed, text = client.call("run_command", target="ops", name="greet")
        assert failed and "greet needs the input who" in text, text
