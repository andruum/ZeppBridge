#!/usr/bin/env python3
"""Authenticated, private MCP control plane for on-demand incremental Zepp sync.

This process is intentionally separate from the read-only ZeppBridge MCP. It
holds cloud credentials and launches only the fixed CLI sync command; callers
cannot choose a mode, a date range, or a shell command. Long syncs run in a
single background worker so MCP requests return before client timeouts.
"""
from __future__ import annotations

import hmac
import json
import os
import socket
import subprocess
import threading
import time
import uuid
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Callable

MAX_REQUEST_BYTES = 1024 * 1024
SOCKET_TIMEOUT_SECONDS = 10
MAX_CONNECTIONS = 24
SYNC_TIMEOUT_SECONDS = 900
SYNC_COOLDOWN_SECONDS = 60
SYNC_COMMAND = ("/usr/local/bin/zeppbridge-cli", "sync", "--mode", "incremental", "--json")
SYNC_ENV_KEYS = (
    "ZEPPBRIDGE_DATA_DIR", "ZEPPBRIDGE_CREDENTIAL_STORE", "ZEPPBRIDGE_APP_TOKEN",
    "ZEPPBRIDGE_USER_ID", "ZEPPBRIDGE_REGION_HOST", "TZ", "HOME",
)
SUPPORTED_VERSIONS = {"2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"}
SERVER_INFO = {"name": "zeppbridge-sync", "version": "1.0.0"}
MODERN_VERSION_KEY = "io.modelcontextprotocol/protocolVersion"
MODERN_INFO_KEY = "io.modelcontextprotocol/serverInfo"

TOOLS = [
    {
        "name": "sync_zepp",
        "description": "Start one incremental Zepp Cloud -> local ZeppBridge sync. Returns a job ID immediately; concurrent or rapid requests reuse the current job. Use get_sync_status to check completion.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
    {
        "name": "get_sync_status",
        "description": "Read the latest on-demand Zepp sync job's progress and sanitized stream counts. A completed job is not proof that any new sleep record exists; query the read-only MCP separately.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
]


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def _safe_report(output: str) -> dict | None:
    try:
        report = json.loads(output)
    except (ValueError, TypeError):
        return None
    if not isinstance(report, dict):
        return None
    streams = report.get("streams", [])
    if not isinstance(streams, list):
        return None
    return {
        "success": report.get("success") is True and report.get("ok") is True,
        "records_written": report.get("recordsWritten") if isinstance(report.get("recordsWritten"), int) else None,
        "streams": [
            {
                "stream": item.get("stream"),
                "status": item.get("status"),
                "records_written": item.get("recordsWritten"),
            }
            for item in streams
            if isinstance(item, dict)
            and isinstance(item.get("stream"), str)
            and isinstance(item.get("status"), str)
            and isinstance(item.get("recordsWritten"), int)
        ],
    }


class SyncManager:
    def __init__(
        self,
        runner: Callable[..., subprocess.CompletedProcess] = subprocess.run,
        cooldown_seconds: int = SYNC_COOLDOWN_SECONDS,
    ) -> None:
        self.runner = runner
        self.cooldown_seconds = cooldown_seconds
        self._lock = threading.Lock()
        self._last_finished = 0.0
        self._job: dict | None = None
        self._worker: threading.Thread | None = None

    def status(self) -> dict:
        with self._lock:
            return dict(self._job) if self._job is not None else {"status": "idle"}

    def start(self) -> dict:
        with self._lock:
            now = time.monotonic()
            if self._job is not None and (
                self._job["status"] == "running" or now - self._last_finished < self.cooldown_seconds
            ):
                return {**self._job, "coalesced": True}
            job = {"job_id": uuid.uuid4().hex, "status": "running", "started_at": utc_now()}
            self._job = job
            self._worker = threading.Thread(target=self._execute, args=(job["job_id"],), daemon=True)
            try:
                self._worker.start()
            except RuntimeError:
                self._job.update({"status": "failed", "error": "sync_start_failed", "finished_at": utc_now()})
                self._worker = None
            return dict(self._job)

    def _execute(self, job_id: str) -> None:
        try:
            result = self.runner(
                SYNC_COMMAND, capture_output=True, text=True, timeout=SYNC_TIMEOUT_SECONDS, check=False,
                env={key: os.environ[key] for key in SYNC_ENV_KEYS if key in os.environ},
            )
            report = _safe_report(result.stdout)
            code = result.returncode
            if code == 0 and report is not None and report["success"]:
                status = "complete"
            elif code == 4:
                status = "busy"  # CLI's cross-process write lock is held by another sync.
            elif code == 8:
                status = "partial"
            else:
                status = "failed"
            outcome = {"status": status, "exit_code": code}
            if report is not None:
                outcome.update(report)
        except subprocess.TimeoutExpired:
            outcome = {"status": "failed", "error": "sync_timeout"}
        except Exception:
            # Unexpected runner failures must never strand the single-flight job.
            outcome = {"status": "failed", "error": "sync_execution_failed"}
        with self._lock:
            if self._job is not None and self._job["job_id"] == job_id:
                self._job.update(outcome)
                self._job["finished_at"] = utc_now()
                self._last_finished = time.monotonic()


def _tool_result(payload: dict) -> dict:
    return {
        "content": [{"type": "text", "text": json.dumps(payload, separators=(",", ":"))}],
        "structuredContent": payload,
        "isError": payload.get("status") in {"failed", "partial", "busy"},
    }


def _modern_result(result: dict) -> dict:
    return {**result, "resultType": "complete", "_meta": {MODERN_INFO_KEY: SERVER_INFO}}


def handle_rpc(request: dict, manager: SyncManager) -> dict:
    method = request.get("method")
    params = request.get("params", {})
    meta = params.get("_meta", {}) if isinstance(params, dict) else {}
    version = meta.get(MODERN_VERSION_KEY) if isinstance(meta, dict) else None
    if version is not None and (not isinstance(version, str) or version not in SUPPORTED_VERSIONS):
        raise RpcError(-32022, "Unsupported MCP protocol version")
    modern = version is not None or method == "server/discover"
    if method == "server/discover":
        return _modern_result({"supportedVersions": sorted(SUPPORTED_VERSIONS, reverse=True),
                               "capabilities": {"tools": {}}, "ttlMs": 3_600_000})
    if method == "initialize":
        if modern:
            raise RpcError(-32601, "initialize is not available in modern MCP")
        wanted = params.get("protocolVersion") if isinstance(params, dict) else None
        return {
            "protocolVersion": wanted if wanted in SUPPORTED_VERSIONS else "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": SERVER_INFO,
            "instructions": "One fixed incremental sync command, no arbitrary command execution. Query the read-only zeppbridge MCP for sleep data after completion.",
        }
    if method == "ping":
        if modern:
            raise RpcError(-32601, "ping is not available in modern MCP")
        return {}
    if method == "tools/list":
        result = {"tools": TOOLS}
        return _modern_result({**result, "ttlMs": 3_600_000}) if modern else result
    if method == "tools/call":
        if not isinstance(params, dict) or not isinstance(params.get("arguments", {}), dict):
            raise RpcError(-32602, "Tool arguments must be an object")
        if params.get("arguments", {}):
            raise RpcError(-32602, "This tool takes no arguments")
        name = params.get("name")
        if name == "sync_zepp":
            result = _tool_result(manager.start())
        elif name == "get_sync_status":
            result = _tool_result(manager.status())
        else:
            raise RpcError(-32602, "Unknown tool")
        return _modern_result(result) if modern else result
    raise RpcError(-32601, "Unknown method")


class RpcError(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


class BoundedHTTPServer(ThreadingHTTPServer):
    request_queue_size = MAX_CONNECTIONS
    daemon_threads = True

    def __init__(self, address, handler) -> None:
        self._slots = threading.BoundedSemaphore(MAX_CONNECTIONS)
        super().__init__(address, handler)

    def process_request(self, request, client_address) -> None:
        if not self._slots.acquire(blocking=False):
            try:
                if isinstance(request, socket.socket):
                    request.settimeout(1)
                    request.sendall(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            except OSError:
                pass
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except Exception:
            self._slots.release()
            raise

    def process_request_thread(self, request, client_address) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._slots.release()


def make_handler(manager: SyncManager, token: str) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        mcp_response_version: str | None = None

        def setup(self) -> None:
            super().setup()
            self.connection.settimeout(SOCKET_TIMEOUT_SECONDS)

        def log_message(self, format: str, *args: object) -> None:
            # Do not write bearer headers or personal health details to logs.
            pass

        def _send(self, code: int, payload: dict | None = None) -> None:
            data = json.dumps(payload).encode("utf-8") if payload is not None else b""
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            if self.mcp_response_version is not None:
                self.send_header("MCP-Protocol-Version", self.mcp_response_version)
            self.end_headers()
            if data:
                self.wfile.write(data)

        def do_GET(self) -> None:
            if self.path == "/healthz":
                self._send(200, {"ok": True})
            else:
                self._send(405)

        def do_POST(self) -> None:
            if self.path != "/mcp":
                self._send(404)
                return
            supplied = self.headers.get("Authorization", "")
            if not hmac.compare_digest(supplied, f"Bearer {token}"):
                self._send(401)
                return
            media_type = self.headers.get("Content-Type", "").split(";", 1)[0].strip().lower()
            if media_type != "application/json":
                self._send(415)
                return
            accept = self.headers.get("Accept", "")
            if accept and not any(part.split(";", 1)[0].strip().lower() in ("application/json", "*/*")
                                  for part in accept.split(",")):
                self._send(406)
                return
            requested_version = self.headers.get("MCP-Protocol-Version")
            if requested_version and requested_version not in SUPPORTED_VERSIONS:
                self._send(400)
                return
            self.mcp_response_version = requested_version or "2025-03-26"
            try:
                size = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                self._send(400)
                return
            if size < 1 or size > MAX_REQUEST_BYTES:
                self._send(413 if size > MAX_REQUEST_BYTES else 400)
                return
            try:
                request = json.loads(self.rfile.read(size))
            except TimeoutError:
                self._send(408)
                return
            except (UnicodeDecodeError, ValueError):
                self._send(400, {"jsonrpc": "2.0", "id": None,
                                  "error": {"code": -32700, "message": "Invalid JSON"}})
                return
            if (not isinstance(request, dict) or request.get("jsonrpc") != "2.0"
                    or not isinstance(request.get("method"), str)
                    or ("id" in request and (type(request["id"]) not in (str, int)))):
                self._send(400, {"jsonrpc": "2.0", "id": None,
                                  "error": {"code": -32600, "message": "Invalid request"}})
                return
            params = request.get("params", {})
            meta = params.get("_meta", {}) if isinstance(params, dict) else {}
            meta_version = meta.get(MODERN_VERSION_KEY) if isinstance(meta, dict) else None
            if isinstance(meta_version, str) and meta_version in SUPPORTED_VERSIONS:
                self.mcp_response_version = meta_version
            if "id" not in request:  # MCP notification; do not execute a side-effecting tool.
                self._send(202)
                return
            try:
                result = handle_rpc(request, manager)
                if isinstance(result.get("protocolVersion"), str):
                    self.mcp_response_version = result["protocolVersion"]
                payload = {"jsonrpc": "2.0", "id": request["id"], "result": result}
            except RpcError as error:
                payload = {"jsonrpc": "2.0", "id": request["id"],
                           "error": {"code": error.code, "message": str(error)}}
            self._send(200, payload)

    return Handler


def main() -> None:
    token = os.environ.get("ZEPPBRIDGE_MCP_AUTH_TOKEN", "")
    if not token.strip():
        raise SystemExit("ZEPPBRIDGE_MCP_AUTH_TOKEN is required")
    host, sep, port = os.environ.get("ZEPPBRIDGE_SYNC_MCP_ADDR", "127.0.0.1:8081").rpartition(":")
    if not sep or not host:
        raise SystemExit("ZEPPBRIDGE_SYNC_MCP_ADDR must be host:port")
    server = BoundedHTTPServer((host, int(port)), make_handler(SyncManager(), token))
    print(f"ZeppBridge sync MCP listening on {host}:{port}", flush=True)
    server.serve_forever(poll_interval=0.5)


if __name__ == "__main__":
    main()
