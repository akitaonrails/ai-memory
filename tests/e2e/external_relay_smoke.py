#!/usr/bin/env python3
"""Deterministic end-to-end smoke test for the external lifecycle relay.

Run with:

    uv run --no-project python tests/e2e/external_relay_smoke.py \
      --ai-memory-bin /absolute/ai-memory \
      --relay-bin /absolute/ai-memory-relay

The test starts only isolated local processes and gives them a minimal,
synthetic environment. Every invocation preserves its temporary root, including
logs, queue files, payload fixtures, and a redacted diagnostic summary. Pass
``--artifacts-parent DIR`` to choose the parent directory. Exit status zero
means every assertion in the smoke test passed.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import contextlib
import http.client
import http.server
import json
import os
import socket
import sqlite3
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import Any, Iterable


WORKSPACE = "external-relay-e2e"
PROJECT = "relay-smoke"
ACTOR = "relay-test-actor"
AGENT = "claude-code"
GOOD_TOKEN = "FAKEfake0123456789relaygood"
BAD_TOKEN = "FAKEfake0123456789relaywrong"
MARKER = "external_relay_deterministic_handoff_marker"
EVENTS = ("session-start", "user-prompt-submit", "session-end")


class SmokeFailure(RuntimeError):
    """An assertion or subprocess failed."""


class DropFirstResponseProxy:
    """Forward requests to the real server and drop responses until released."""

    def __init__(self, target_port: int, timeout_seconds: float) -> None:
        self.target_port = target_port
        self.timeout_seconds = timeout_seconds
        self.mode_lock = threading.Lock()
        self.drop_responses = True
        proxy = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                headers = {
                    key: value
                    for key, value in self.headers.items()
                    if key.lower() not in {"host", "connection", "content-length"}
                }
                upstream = http.client.HTTPConnection(
                    "127.0.0.1",
                    proxy.target_port,
                    timeout=proxy.timeout_seconds,
                )
                try:
                    upstream.request("POST", self.path, body=body, headers=headers)
                    response = upstream.getresponse()
                    response_body = response.read()
                    with proxy.mode_lock:
                        drop = proxy.drop_responses
                    if drop:
                        self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                        return
                    self.send_response(response.status)
                    for key, value in response.getheaders():
                        if key.lower() not in {
                            "connection",
                            "content-length",
                            "transfer-encoding",
                        }:
                            self.send_header(key, value)
                    self.send_header("Content-Length", str(len(response_body)))
                    self.end_headers()
                    self.wfile.write(response_body)
                finally:
                    upstream.close()

            def log_message(self, _format: str, *_args: Any) -> None:
                return

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(
            target=self.server.serve_forever,
            name="external-relay-drop-response-proxy",
            daemon=True,
        )
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise SmokeFailure("fault proxy thread did not stop")

    def forward_responses(self) -> None:
        with self.mode_lock:
            self.drop_responses = False


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=(
            "Run an isolated real-server E2E test for ai-memory-relay. All "
            "artifacts are retained; exit status 0 means every assertion passed."
        ),
        epilog=(
            "Example: uv run --no-project python tests/e2e/external_relay_smoke.py "
            "--ai-memory-bin /absolute/ai-memory "
            "--relay-bin /absolute/ai-memory-relay"
        ),
    )
    parser.add_argument(
        "--ai-memory-bin",
        required=True,
        type=Path,
        help="absolute path to the ai-memory executable",
    )
    parser.add_argument(
        "--relay-bin",
        required=True,
        type=Path,
        help="absolute path to the ai-memory-relay executable",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=float,
        default=60.0,
        help="bounded timeout for readiness and each subprocess (default: 60)",
    )
    parser.add_argument(
        "--artifacts-parent",
        type=Path,
        help="existing directory under which the retained temp root is created",
    )
    args = parser.parse_args()
    if args.timeout_seconds < 1:
        parser.error("--timeout-seconds must be at least 1")
    for option in ("ai_memory_bin", "relay_bin"):
        value = getattr(args, option)
        if not value.is_absolute():
            parser.error(f"--{option.replace('_', '-')} must be an absolute path")
        if not value.is_file() or not os.access(value, os.X_OK):
            parser.error(f"--{option.replace('_', '-')} is not an executable file: {value}")
    if args.artifacts_parent is not None:
        args.artifacts_parent = args.artifacts_parent.resolve()
        if not args.artifacts_parent.is_dir():
            parser.error("--artifacts-parent must name an existing directory")
    return args


def reserve_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def write_json(path: Path, value: Any) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def parse_status(stdout: str, label: str) -> dict[str, int]:
    def unique_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise SmokeFailure(f"{label} status JSON duplicated field {key}")
            result[key] = value
        return result

    try:
        parsed = json.loads(stdout, object_pairs_hook=unique_object)
    except json.JSONDecodeError as error:
        raise SmokeFailure(f"{label} did not emit one JSON object") from error
    if not isinstance(parsed, dict):
        raise SmokeFailure(f"{label} status JSON is not an object")
    pending_items = parsed.get("pending_items")
    receipts = parsed.get("receipts")
    if type(pending_items) is not int:
        raise SmokeFailure(f"{label} status JSON has no integer pending_items")
    if type(receipts) is not int:
        raise SmokeFailure(f"{label} status JSON has no integer receipts")
    return {"pending_items": pending_items, "receipts": receipts}


class Harness:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        parent = str(args.artifacts_parent) if args.artifacts_parent else None
        self.root = Path(tempfile.mkdtemp(prefix="ai-memory-external-relay-", dir=parent))
        self.home = self.root / "home"
        self.data = self.root / "data"
        self.project = self.root / "project"
        self.queues = self.root / "queues"
        self.payloads = self.root / "payloads"
        self.logs = self.root / "logs"
        self.tmp = self.root / "tmp"
        for directory in (
            self.home,
            self.data,
            self.project,
            self.queues,
            self.payloads,
            self.logs,
            self.tmp,
        ):
            directory.mkdir()
        (self.project / ".ai-memory.toml").write_text(
            f'workspace = "{WORKSPACE}"\nproject = "{PROJECT}"\n', encoding="utf-8"
        )
        self.empty_gitconfig = self.root / "empty-gitconfig"
        self.empty_gitconfig.write_text("", encoding="utf-8")
        self.port = reserve_port()
        self.base_url = f"http://127.0.0.1:{self.port}"
        self.server: subprocess.Popen[bytes] | None = None
        self.server_log_handle: Any = None
        self.proxies: list[DropFirstResponseProxy] = []
        self.command_index = 0
        self.command_lock = threading.Lock()
        self.checks: dict[str, Any] = {}

    def environment(self, token: str | None = GOOD_TOKEN) -> dict[str, str]:
        env = {
            "HOME": str(self.home),
            "USERPROFILE": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_DATA_HOME": str(self.home / ".local" / "share"),
            "TMPDIR": str(self.tmp),
            "TEMP": str(self.tmp),
            "TMP": str(self.tmp),
            "PATH": os.defpath,
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": str(self.empty_gitconfig),
            "GIT_CONFIG_SYSTEM": str(self.empty_gitconfig),
            "AI_MEMORY_HOME": str(self.home),
            "AI_MEMORY_DATA_DIR": str(self.data),
            "AI_MEMORY_EMBEDDING_PROVIDER": "none",
            "AI_MEMORY_BACKFILL_ON_START": "false",
            "AI_MEMORY_CONSOLIDATE_ON_SESSION_END": "false",
            "AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED": "false",
            "RUST_LOG": "off",
        }
        if os.name == "nt":
            for key in ("SystemRoot", "WINDIR", "SystemDrive"):
                value = os.environ.get(key)
                if value:
                    env[key] = value
        if token is not None:
            env["AI_MEMORY_AUTH_TOKEN"] = token
        return env

    def run(
        self,
        binary: Path,
        arguments: Iterable[str | Path],
        *,
        token: str | None = GOOD_TOKEN,
        input_text: str | None = None,
        expect: int | None = 0,
        label: str,
    ) -> subprocess.CompletedProcess[str]:
        command = [str(binary), *(str(arg) for arg in arguments)]
        started = time.monotonic()
        try:
            result = subprocess.run(
                command,
                cwd=self.project,
                env=self.environment(token),
                input=input_text,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=self.args.timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise SmokeFailure(f"{label} exceeded {self.args.timeout_seconds:g}s") from error
        with self.command_lock:
            self.command_index += 1
            log_path = self.logs / f"command-{self.command_index:03d}.json"
        write_json(
            log_path,
            {
                "label": label,
                "argv": command,
                "duration_seconds": round(time.monotonic() - started, 3),
                "returncode": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            },
        )
        if expect is not None and result.returncode != expect:
            raise SmokeFailure(f"{label} exited {result.returncode}, expected {expect}")
        return result

    def relay(self, arguments: Iterable[str | Path], **kwargs: Any) -> subprocess.CompletedProcess[str]:
        return self.run(self.args.relay_bin, arguments, **kwargs)

    def queue_init(
        self, name: str, producer: str, server_url: str | None = None
    ) -> Path:
        queue = self.queues / name
        self.relay(
            [
                "init",
                "--queue-dir",
                queue,
                "--server-url",
                server_url or self.base_url,
                "--producer",
                producer,
                "--actor",
                ACTOR,
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
            ],
            label=f"init-{name}",
        )
        return queue

    def status(self, queue: Path, label: str) -> tuple[Any, int]:
        result = self.relay(["status", "--queue-dir", queue], label=label)
        parsed = parse_status(result.stdout, label)
        return parsed, parsed["pending_items"]

    def enqueue(self, queue: Path, payload_file: Path, label: str) -> None:
        self.relay(
            ["enqueue", "--queue-dir", queue, "--file", payload_file], label=label
        )

    def flush(
        self,
        queue: Path,
        label: str,
        token: str = GOOD_TOKEN,
        expect: int = 0,
    ) -> int:
        return self.relay(
            ["flush", "--queue-dir", queue],
            token=token,
            expect=expect,
            label=label,
        ).returncode

    def start_server(self) -> None:
        server_log = self.logs / "server.log"
        self.server_log_handle = server_log.open("xb")
        self.server = subprocess.Popen(
            [
                str(self.args.ai_memory_bin),
                "--data-dir",
                str(self.data),
                "serve",
                "--transport",
                "http",
                "--bind",
                f"127.0.0.1:{self.port}",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--no-watcher",
            ],
            cwd=self.project,
            env=self.environment(GOOD_TOKEN),
            stdin=subprocess.DEVNULL,
            stdout=self.server_log_handle,
            stderr=subprocess.STDOUT,
        )
        deadline = time.monotonic() + self.args.timeout_seconds
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                raise SmokeFailure(f"ai-memory server exited {self.server.returncode} before readiness")
            request = urllib.request.Request(
                f"{self.base_url}/mcp",
                headers={"Authorization": f"Bearer {GOOD_TOKEN}"},
            )
            try:
                with urllib.request.urlopen(request, timeout=0.5):
                    return
            except urllib.error.HTTPError as error:
                if error.code in (400, 404, 405):
                    return
            except (urllib.error.URLError, TimeoutError):
                pass
            time.sleep(0.05)
        raise SmokeFailure("ai-memory server readiness timeout")

    def stop_server(self) -> None:
        process = self.server
        if process is None:
            return
        self.server = None
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=min(5.0, self.args.timeout_seconds))
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=min(5.0, self.args.timeout_seconds))
        if self.server_log_handle is not None:
            self.server_log_handle.close()
            self.server_log_handle = None

    def start_drop_response_proxy(self) -> DropFirstResponseProxy:
        proxy = DropFirstResponseProxy(self.port, self.args.timeout_seconds)
        self.proxies.append(proxy)
        return proxy

    def stop_proxies(self) -> None:
        while self.proxies:
            self.proxies.pop().stop()

    def db(self) -> sqlite3.Connection:
        path = self.data / "db" / "memory.sqlite"
        return sqlite3.connect(f"file:{path}?mode=ro", uri=True)

    def observation_count(self) -> int:
        with contextlib.closing(self.db()) as connection:
            return int(connection.execute("SELECT COUNT(*) FROM observations").fetchone()[0])

    def hook_spool_json_count(self) -> int:
        spool = self.data / "hook-spool"
        if not spool.is_dir():
            return 0
        return sum(1 for path in spool.rglob("*.json") if path.is_file())

    def observations(self, session_id: uuid.UUID) -> list[tuple[str, str | None, str | None, str]]:
        with contextlib.closing(self.db()) as connection:
            return list(
                connection.execute(
                    "SELECT kind, extension, source_event, body FROM observations "
                    "WHERE session_id = ? ORDER BY created_at, rowid",
                    (session_id.bytes,),
                )
            )

    def handoff_state(self, from_session: uuid.UUID) -> str | None:
        with contextlib.closing(self.db()) as connection:
            row = connection.execute(
                "SELECT state FROM handoffs WHERE from_session_id = ? ORDER BY created_at DESC LIMIT 1",
                (from_session.bytes,),
            ).fetchone()
            return None if row is None else str(row[0])

    def native_hook(self, event: str, session_id: uuid.UUID) -> subprocess.CompletedProcess[str]:
        payload = json.dumps(
            {
                "session_id": str(session_id),
                "cwd": str(self.project),
                "prompt": "native duplicate must remain uncaptured",
            }
        )
        env = self.environment(GOOD_TOKEN)
        env["AI_MEMORY_CAPTURE_OWNER"] = "external-relay-e2e"
        command = [
            str(self.args.ai_memory_bin),
            "--data-dir",
            str(self.data),
            "hook",
            "--event",
            event,
            "--agent",
            AGENT,
            "--server-url",
            self.base_url,
            "--auth-token",
            GOOD_TOKEN,
        ]
        try:
            result = subprocess.run(
                command,
                cwd=self.project,
                env=env,
                input=payload,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=self.args.timeout_seconds,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            raise SmokeFailure(f"native-{event} exceeded timeout") from error
        self.command_index += 1
        write_json(
            self.logs / f"command-{self.command_index:03d}.json",
            {
                "label": f"native-{event}",
                "argv": [arg if arg != GOOD_TOKEN else "<redacted>" for arg in command],
                "returncode": result.returncode,
                "stdout": result.stdout,
                "stderr": result.stderr,
            },
        )
        if result.returncode != 0:
            raise SmokeFailure(f"native-{event} exited {result.returncode}")
        return result


def lifecycle(session_id: uuid.UUID, prefix: str, prompt: str = MARKER) -> list[dict[str, Any]]:
    return [
        {
            "event_id": f"{prefix}-start",
            "agent": AGENT,
            "event": "session-start",
            "body": {"session_id": str(session_id), "cwd": "", "source": prefix},
        },
        {
            "event_id": f"{prefix}-prompt",
            "agent": AGENT,
            "event": "user-prompt-submit",
            "body": {"session_id": str(session_id), "cwd": "", "prompt": prompt},
        },
        {
            "event_id": f"{prefix}-end",
            "agent": AGENT,
            "event": "session-end",
            "body": {"session_id": str(session_id), "cwd": "", "reason": "completed"},
        },
    ]


def materialize(harness: Harness, name: str, events: list[dict[str, Any]]) -> Path:
    copied = json.loads(json.dumps(events))
    for item in copied:
        item["body"]["cwd"] = str(harness.project)
    path = harness.payloads / f"{name}.json"
    write_json(path, copied)
    return path


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SmokeFailure(message)


def run_smoke(harness: Harness) -> None:
    main_session = uuid.uuid4()
    main_queue = harness.queue_init("offline-main", "producer-a")
    initial_status, initial_pending = harness.status(main_queue, "status-initial")
    require(initial_pending == 0, f"new queue has {initial_pending} pending events")

    canonical_file = materialize(
        harness, "canonical-offline", lifecycle(main_session, "canonical")
    )
    harness.enqueue(main_queue, canonical_file, "enqueue-offline")
    _, offline_pending = harness.status(main_queue, "status-offline-enqueued")
    require(offline_pending == 3, f"offline enqueue retained {offline_pending}, expected 3")

    harness.start_server()
    harness.flush(main_queue, "flush-after-server-start")
    _, flushed_pending = harness.status(main_queue, "status-after-flush")
    require(flushed_pending == 0, f"successful flush left {flushed_pending} pending")
    main_rows = harness.observations(main_session)
    require([row[0] for row in main_rows] == ["session-start", "user-prompt", "session-end"], "canonical lifecycle order differs")
    require(all(row[1] == "producer-a" for row in main_rows), "canonical producer extension differs")
    require([row[2] for row in main_rows] == list(EVENTS), "canonical source events differ")

    before_native = harness.observation_count()
    spool_before_native = harness.hook_spool_json_count()
    receiver = uuid.uuid4()
    native_start = harness.native_hook("session-start", receiver)
    require(MARKER in native_start.stdout, "capture-owner SessionStart did not receive handoff")
    harness.native_hook("user-prompt-submit", receiver)
    harness.native_hook("session-end", receiver)
    after_native = harness.observation_count()
    spool_after_native = harness.hook_spool_json_count()
    require(after_native == before_native, "capture-owner native hooks created observations")
    require(
        spool_after_native == spool_before_native,
        "capture-owner native hooks created spool JSON files",
    )
    require(harness.handoff_state(main_session) == "accepted", "capture-owner hook did not preserve and accept the external handoff")

    harness.enqueue(main_queue, canonical_file, "enqueue-identical-replay")
    harness.flush(main_queue, "flush-identical-replay")
    require(len(harness.observations(main_session)) == 3, "identical replay duplicated observations")

    collision_before = harness.observation_count()
    collision_queue = harness.queue_init(
        "collision-foreign-agent", "producer-collision"
    )
    collision_status_before, _ = harness.status(
        collision_queue, "status-before-collision"
    )
    collision_receipts_before = collision_status_before["receipts"]
    collision_file = materialize(
        harness,
        "session-collision",
        [
            {
                "event_id": "wrong-agent-collision",
                "agent": "codex",
                "event": "user-prompt-submit",
                "body": {
                    "session_id": str(main_session),
                    "cwd": "",
                    "prompt": "valid wrong-agent event expected to be acknowledged and dropped",
                },
            }
        ],
    )
    harness.enqueue(collision_queue, collision_file, "enqueue-session-collision")
    harness.flush(collision_queue, "flush-session-collision")
    collision_status_after, collision_pending = harness.status(
        collision_queue, "status-after-collision"
    )
    collision_receipts_after = collision_status_after["receipts"]
    require(collision_pending == 0, "acknowledged SessionCollision remained pending")
    require(
        collision_receipts_after == collision_receipts_before + 1,
        "SessionCollision acknowledgement did not create one relay receipt",
    )
    collision_stored_delta = harness.observation_count() - collision_before
    require(collision_stored_delta == 0, "SessionCollision created a stored observation")

    same_body = "identical body with distinct external identities"
    producer_a_session = uuid.uuid4()
    producer_a_file = materialize(
        harness,
        "distinct-producer-a",
        [
            {
                "event_id": "shared-event-id",
                "agent": AGENT,
                "event": "user-prompt-submit",
                "body": {"session_id": str(producer_a_session), "cwd": "", "prompt": same_body},
            },
            {
                "event_id": "different-event-id",
                "agent": AGENT,
                "event": "user-prompt-submit",
                "body": {"session_id": str(producer_a_session), "cwd": "", "prompt": same_body},
            },
        ],
    )
    harness.enqueue(main_queue, producer_a_file, "enqueue-distinct-event-ids")
    harness.flush(main_queue, "flush-distinct-event-ids")
    producer_a_rows = harness.observations(producer_a_session)
    require(len(producer_a_rows) == 2, "distinct event IDs collapsed")

    producer_b_queue = harness.queue_init("producer-b", "producer-b")
    producer_b_file = materialize(
        harness,
        "distinct-producer-b",
        [
            {
                "event_id": "shared-event-id",
                "agent": AGENT,
                "event": "user-prompt-submit",
                "body": {"session_id": str(producer_a_session), "cwd": "", "prompt": same_body},
            }
        ],
    )
    harness.enqueue(producer_b_queue, producer_b_file, "enqueue-distinct-producer")
    harness.flush(producer_b_queue, "flush-distinct-producer")
    combined_producer_rows = harness.observations(producer_a_session)
    require(len(combined_producer_rows) == 3, "distinct producer collapsed")
    require(
        sum(row[1] == "producer-a" for row in combined_producer_rows) == 2
        and sum(row[1] == "producer-b" for row in combined_producer_rows) == 1,
        "producer namespaces were not preserved",
    )

    proxy = harness.start_drop_response_proxy()
    lost_response_session = uuid.uuid4()
    lost_response_queue = harness.queue_init(
        "lost-response", "producer-loss", server_url=proxy.url
    )
    lost_response_file = materialize(
        harness,
        "lost-response",
        [
            {
                "event_id": "accepted-before-response-loss",
                "agent": AGENT,
                "event": "user-prompt-submit",
                "body": {
                    "session_id": str(lost_response_session),
                    "cwd": "",
                    "prompt": "response loss idempotency marker",
                },
            }
        ],
    )
    harness.enqueue(lost_response_queue, lost_response_file, "enqueue-lost-response")
    lost_response_exit = harness.flush(
        lost_response_queue, "flush-lost-response", expect=2
    )
    _, lost_response_pending = harness.status(
        lost_response_queue, "status-lost-response"
    )
    require(lost_response_pending == 1, "response loss did not retain the queue item")
    require(
        len(harness.observations(lost_response_session)) == 1,
        "real server did not accept exactly one observation before response loss",
    )
    proxy.forward_responses()
    harness.flush(lost_response_queue, "flush-lost-response-retry")
    _, lost_response_retry_pending = harness.status(
        lost_response_queue, "status-lost-response-retry"
    )
    require(lost_response_retry_pending == 0, "lost-response retry remained pending")
    require(
        len(harness.observations(lost_response_session)) == 1,
        "lost-response retry duplicated the real-server observation",
    )

    concurrent_sessions: list[tuple[uuid.UUID, Path]] = []
    for index in range(15):
        session_id = uuid.uuid4()
        queue = harness.queue_init(f"concurrent-{index:02d}", f"parallel-{index % 3}")
        payload = materialize(
            harness,
            f"concurrent-{index:02d}",
            lifecycle(session_id, f"parallel-{index:02d}", prompt=f"parallel marker {index}"),
        )
        harness.enqueue(queue, payload, f"enqueue-concurrent-{index:02d}")
        concurrent_sessions.append((session_id, queue))
    with concurrent.futures.ThreadPoolExecutor(max_workers=15) as executor:
        futures = [
            executor.submit(harness.flush, queue, f"flush-concurrent-{index:02d}")
            for index, (_, queue) in enumerate(concurrent_sessions)
        ]
        for future in futures:
            future.result(timeout=harness.args.timeout_seconds + 5)
    for session_id, queue in concurrent_sessions:
        rows = harness.observations(session_id)
        require([row[0] for row in rows] == ["session-start", "user-prompt", "session-end"], f"concurrent session {session_id} lifecycle order differs")
        _, pending = harness.status(queue, f"status-concurrent-{session_id.hex[:8]}")
        require(pending == 0, f"concurrent queue {session_id} retained {pending}")

    auth_session = uuid.uuid4()
    auth_queue = harness.queue_init("auth-retry", "producer-auth")
    auth_file = materialize(
        harness,
        "auth-retry",
        [
            {
                "event_id": "auth-event",
                "agent": AGENT,
                "event": "user-prompt-submit",
                "body": {"session_id": str(auth_session), "cwd": "", "prompt": "auth retry marker"},
            }
        ],
    )
    harness.enqueue(auth_queue, auth_file, "enqueue-auth-retry")
    bad_code = harness.flush(
        auth_queue, "flush-auth-rejected", token=BAD_TOKEN, expect=2
    )
    _, rejected_pending = harness.status(auth_queue, "status-auth-rejected")
    require(rejected_pending == 1, f"auth rejection retained {rejected_pending}, expected 1")
    require(len(harness.observations(auth_session)) == 0, "unauthorized flush wrote an observation")
    harness.flush(auth_queue, "flush-auth-retry-correct")
    _, retry_pending = harness.status(auth_queue, "status-auth-retry-correct")
    require(retry_pending == 0, f"authorized retry left {retry_pending} pending")
    require(len(harness.observations(auth_session)) == 1, "authorized retry lost or duplicated event")

    final_count = harness.observation_count()
    expected_count = 3 + 2 + 1 + 1 + (15 * 3) + 1
    require(final_count == expected_count, f"final observation count {final_count}, expected {expected_count}")
    harness.checks = {
        "initial_queue_status": initial_status,
        "initial_pending": initial_pending,
        "offline_pending": offline_pending,
        "offline_restart_flush": "passed",
        "canonical_lifecycle": "passed",
        "capture_owner_zero_new_observations": after_native - before_native,
        "capture_owner_new_spool_json": spool_after_native - spool_before_native,
        "handoff_state": harness.handoff_state(main_session),
        "identical_replay_count": len(harness.observations(main_session)),
        "session_collision": {
            "relay_acknowledged": collision_pending == 0,
            "receipt_delta": collision_receipts_after - collision_receipts_before,
            "stored_observation_delta": collision_stored_delta,
        },
        "distinct_event_id_count": len(producer_a_rows),
        "distinct_producer_count": sum(
            row[1] == "producer-b" for row in combined_producer_rows
        ),
        "lost_response": {
            "first_exit": lost_response_exit,
            "pending_after_loss": lost_response_pending,
            "pending_after_retry": lost_response_retry_pending,
            "stored_observations": len(
                harness.observations(lost_response_session)
            ),
        },
        "concurrent_sessions": len(concurrent_sessions),
        "concurrent_observations": sum(
            len(harness.observations(sid)) for sid, _ in concurrent_sessions
        ),
        "auth_rejected_exit": bad_code,
        "auth_rejected_pending": rejected_pending,
        "auth_retry_pending": retry_pending,
        "final_observations": final_count,
        "expected_observations": expected_count,
    }


def main() -> int:
    args = parse_args()
    harness = Harness(args)
    passed = False
    error_message: str | None = None
    try:
        run_smoke(harness)
        passed = True
        return 0
    except Exception as error:  # diagnostics must survive every assertion failure
        error_message = f"{type(error).__name__}: {error}"
        return 1
    finally:
        try:
            harness.stop_proxies()
        finally:
            harness.stop_server()
        summary = {
            "status": "passed" if passed else "failed",
            "artifacts": str(harness.root),
            "checks": harness.checks,
        }
        if error_message is not None:
            summary["error"] = error_message
        write_json(harness.root / "summary.json", summary)
        print(json.dumps(summary, sort_keys=True))


if __name__ == "__main__":
    raise SystemExit(main())
