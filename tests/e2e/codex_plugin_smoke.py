#!/usr/bin/env python3
"""Sandboxed end-to-end test of the ai-memory Codex plugin (#1166).

Run with:

    uv run --no-project python tests/e2e/codex_plugin_smoke.py \
      --ai-memory-bin /absolute/ai-memory --repo-root .

It needs the `codex` CLI on PATH and no OpenAI credentials. Everything runs
in a scratch HOME and CODEX_HOME; the model provider points at a closed
loopback port, so no request leaves the machine. Steps:

1. publish `plugins/codex` and the Codex catalog into a throwaway git repo
   (the committed catalog pins a release tag that may not exist yet), then
   `codex plugin marketplace add` it and `codex plugin add ai-memory@ai-memory`;
2. `codex app-server` reports the plugin's eight hooks, its skills and its
   MCP server, and every hook starts untrusted;
3. after trusting the hooks, a thread and a turn fire the plugin's
   SessionStart and UserPromptSubmit hooks through the launcher into the real
   binary, which spools them as agent codex in the fixture marker's scope;
4. a plugin version bump with unchanged hook commands keeps every hook
   trusted (Codex keys trust on the unexpanded command).
"""

from __future__ import annotations

import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WORKSPACE = "codex-plugin-e2e"
PROJECT = "fixture"


class SmokeFailure(RuntimeError):
    """An assertion failed."""


def check(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeFailure(message)


class Sandbox:
    def __init__(self, root: Path, binary: Path, repo_root: Path) -> None:
        self.root = root
        self.repo_root = repo_root
        self.home = root / "home"
        self.codex_home = self.home / ".codex"
        self.data = root / "data"
        self.bin_dir = root / "bin"
        self.catalog = root / "catalog"
        self.project = root / "project"
        for directory in (self.codex_home, self.bin_dir, self.project):
            directory.mkdir(parents=True, exist_ok=True)
        # The plugin finds the binary on PATH, as for a real install.
        (self.bin_dir / "ai-memory").symlink_to(binary)
        codex = shutil.which("codex")
        if codex is None:
            raise SmokeFailure("the codex CLI is not on PATH")
        self.path = os.pathsep.join(
            [str(self.bin_dir), str(Path(os.path.realpath(codex)).parent), str(Path(codex).parent), "/usr/bin", "/bin"]
        )

    def env(self) -> dict[str, str]:
        return {
            "HOME": str(self.home),
            "PATH": self.path,
            "CODEX_HOME": str(self.codex_home),
            "AI_MEMORY_DATA_DIR": str(self.data),
            # A closed port: events stay in the spool for inspection.
            "AI_MEMORY_SERVER_URL": "http://127.0.0.1:9",
            "LANG": "C.UTF-8",
            "TERM": "dumb",
        }

    def run(self, *args: str) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            list(args), env=self.env(), capture_output=True, text=True, timeout=120, check=False
        )
        check(result.returncode == 0, f"{' '.join(args)} failed: {result.stdout}{result.stderr}")
        return result

    def publish_catalog(self, version_suffix: str = "") -> None:
        """A git repo holding the Codex plugin and a catalog that points at it."""
        plugin = self.catalog / "plugins" / "codex"
        if plugin.exists():
            shutil.rmtree(plugin)
        shutil.copytree(self.repo_root / "plugins" / "codex", plugin, symlinks=True)
        manifest = plugin / ".codex-plugin" / "plugin.json"
        data = json.loads(manifest.read_text(encoding="utf-8"))
        data["version"] = data["version"] + version_suffix
        manifest.write_text(json.dumps(data, indent=2), encoding="utf-8")
        catalog = json.loads((self.repo_root / ".agents/plugins/marketplace.json").read_text(encoding="utf-8"))
        catalog["plugins"][0]["source"]["url"] = self.catalog.as_uri()
        catalog["plugins"][0]["source"]["ref"] = "HEAD"
        target = self.catalog / ".agents" / "plugins" / "marketplace.json"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(json.dumps(catalog, indent=2), encoding="utf-8")
        git = ["git", "-C", str(self.catalog), "-c", "user.name=smoke", "-c", "user.email=smoke@example.invalid"]
        if not (self.catalog / ".git").exists():
            subprocess.run([*git, "init", "-q"], check=True, env=self.env())
        subprocess.run([*git, "add", "-A"], check=True, env=self.env())
        subprocess.run([*git, "commit", "-q", "--no-gpg-sign", "-m", f"publish{version_suffix}"], check=True, env=self.env())

    def app_server(self, requests: list[dict], until: int, timeout: float = 30) -> dict[int, dict]:
        """Send requests to `codex app-server`; collect responses up to id `until`."""
        process = subprocess.Popen(
            ["codex", "app-server"],
            cwd=self.project,
            env=self.env(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        assert process.stdin is not None and process.stdout is not None
        responses: dict[int, dict] = {}
        notifications: list[dict] = []
        pending = list(requests)

        def send_next() -> None:
            while pending:
                message = pending.pop(0)
                process.stdin.write(json.dumps(message) + "\n")
                process.stdin.flush()
                if "id" in message:
                    return

        send_next()
        deadline = time.monotonic() + timeout
        try:
            while time.monotonic() < deadline:
                ready, _, _ = select.select([process.stdout], [], [], 0.5)
                if not ready:
                    continue
                line = process.stdout.readline()
                if not line:
                    break
                message = json.loads(line)
                if "id" in message and ("result" in message or "error" in message):
                    responses[int(message["id"])] = message
                    if int(message["id"]) >= until:
                        break
                    send_next()
                else:
                    notifications.append(message)
        finally:
            process.kill()
            process.wait()
        responses[-1] = {"notifications": notifications}
        return responses

    def thread_and_turn(self, timeout: float = 30) -> None:
        """Start a thread and one turn in a single app-server session; the
        turn fails at the offline model, after the hooks have run."""
        process = subprocess.Popen(
            ["codex", "app-server"],
            cwd=self.project,
            env=self.env(),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        assert process.stdin is not None and process.stdout is not None

        def send(message: dict) -> None:
            process.stdin.write(json.dumps(message) + "\n")
            process.stdin.flush()

        for message in self.init():
            send(message)
        send({"jsonrpc": "2.0", "id": 2, "method": "thread/start", "params": {"cwd": str(self.project)}})
        completed = 0
        deadline = time.monotonic() + timeout
        try:
            while time.monotonic() < deadline and completed < 2:
                ready, _, _ = select.select([process.stdout], [], [], 0.5)
                if not ready:
                    continue
                line = process.stdout.readline()
                if not line:
                    break
                message = json.loads(line)
                if message.get("id") == 2 and "result" in message:
                    thread = message["result"]["thread"]["id"]
                    send(
                        {
                            "jsonrpc": "2.0",
                            "id": 3,
                            "method": "turn/start",
                            "params": {"threadId": thread, "input": [{"type": "text", "text": "hello"}]},
                        }
                    )
                elif message.get("method") == "hook/completed":
                    completed += 1
        finally:
            process.kill()
            process.wait()

    def init(self) -> list[dict]:
        return [
            {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"clientInfo": {"name": "smoke", "version": "0"}}},
            {"jsonrpc": "2.0", "method": "initialized"},
        ]

    def hooks(self) -> list[dict]:
        replies = self.app_server(
            [*self.init(), {"jsonrpc": "2.0", "id": 2, "method": "hooks/list", "params": {"cwds": [str(self.project)]}}],
            until=2,
        )
        check(2 in replies, "hooks/list did not answer")
        data = replies[2]["result"]["data"][0]
        check(not data["errors"], f"hook errors: {data['errors']}")
        return [hook for hook in data["hooks"] if hook.get("pluginId") == "ai-memory@ai-memory"]

    def spooled(self) -> list[str]:
        spool = self.data / "hook-spool"
        if not spool.exists():
            return []
        return [json.loads(path.read_text(encoding="utf-8"))["url"] for path in sorted(spool.glob("*.json"))]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--ai-memory-bin", required=True, type=Path)
    parser.add_argument("--repo-root", required=True, type=Path)
    parser.add_argument("--artifacts-parent", type=Path)
    args = parser.parse_args()
    parent = args.artifacts_parent.resolve() if args.artifacts_parent else None
    root = Path(tempfile.mkdtemp(prefix="ai-memory-codex-plugin-e2e-", dir=parent))
    box = Sandbox(root, args.ai_memory_bin.resolve(), args.repo_root.resolve())
    try:
        (box.project / ".ai-memory.toml").write_text(
            f'workspace = "{WORKSPACE}"\nproject = "{PROJECT}"\n', encoding="utf-8"
        )
        # No request may leave the machine: a model provider on a closed port.
        (box.codex_home / "config.toml").write_text(
            'model_provider = "offline"\n\n[model_providers.offline]\nname = "offline"\n'
            'base_url = "http://127.0.0.1:9/v1"\nwire_api = "responses"\n',
            encoding="utf-8",
        )
        box.publish_catalog()
        box.run("codex", "plugin", "marketplace", "add", str(box.catalog))
        box.run("codex", "plugin", "add", "ai-memory@ai-memory")

        hooks = box.hooks()
        check(len(hooks) == 8, f"expected 8 plugin hooks, saw {len(hooks)}")
        check(all(hook["trustStatus"] == "untrusted" for hook in hooks), "a plugin hook started trusted")
        replies = box.app_server(
            [
                *box.init(),
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "plugin/read",
                    "params": {"marketplacePath": str(box.catalog / ".agents/plugins/marketplace.json"), "pluginName": "ai-memory"},
                },
            ],
            until=2,
        )
        plugin = replies[2]["result"]["plugin"]
        check(len(plugin["skills"]) == 7, f"skills: {len(plugin['skills'])}")
        check(plugin["mcpServers"] == ["ai-memory"], f"mcp: {plugin['mcpServers']}")
        print("ok: the Codex plugin installs with 8 untrusted hooks, 7 skills and the ai-memory MCP server")

        trust = "".join(
            f'\n[hooks.state."{hook["key"]}"]\ntrusted_hash = "{hook["currentHash"]}"\n' for hook in hooks
        )
        with (box.codex_home / "config.toml").open("a", encoding="utf-8") as config:
            config.write(trust)
        check(all(hook["trustStatus"] == "trusted" for hook in box.hooks()), "trust was not applied")

        box.thread_and_turn()
        urls = box.spooled()
        for event in ("session-start", "user-prompt-submit"):
            matching = [url for url in urls if f"event={event}&agent=codex" in url]
            check(len(matching) == 1, f"{event}: {urls}")
            check(f"workspace={WORKSPACE}" in matching[0] and f"project={PROJECT}" in matching[0], matching[0])
        print("ok: trusted plugin hooks spooled session-start and user-prompt-submit as codex in the marker scope")

        box.publish_catalog(version_suffix="-next")
        # A local marketplace is read in place: re-adding installs the bump.
        added = box.run("codex", "plugin", "add", "ai-memory@ai-memory").stdout
        check("-next" in added, f"the bumped version was not installed: {added}")
        statuses = {hook["trustStatus"] for hook in box.hooks()}
        check(statuses == {"trusted"}, f"trust after a plugin upgrade: {statuses}")
        print("ok: hook trust survived a plugin version bump")
    except SmokeFailure as failure:
        print(f"FAIL: {failure} (sandbox kept at {root})", file=sys.stderr)
        return 1
    if parent is None:
        shutil.rmtree(root, ignore_errors=True)
    print("codex plugin smoke: all steps passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
