#!/usr/bin/env python3
"""Private, non-MCP sync worker behind the original ZeppBridge MCP endpoint.

The original MCP calls this worker over a private network. Only this process
receives Zepp Cloud credentials. It accepts no CLI arguments from callers and
runs the fixed incremental sync command with a filtered subprocess environment.
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

MAX_CONNECTIONS = 24
SOCKET_TIMEOUT_SECONDS = 10
SYNC_TIMEOUT_SECONDS = 900
SYNC_COOLDOWN_SECONDS = 60
SYNC_COMMAND = ("/usr/local/bin/zeppbridge-cli", "sync", "--mode", "incremental", "--json")
SYNC_ENV_KEYS = (
    "ZEPPBRIDGE_DATA_DIR", "ZEPPBRIDGE_CREDENTIAL_STORE", "ZEPPBRIDGE_APP_TOKEN",
    "ZEPPBRIDGE_USER_ID", "ZEPPBRIDGE_REGION_HOST", "TZ", "HOME",
)


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
                status = "busy"  # CLI cross-process write lock is held by another sync.
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
            outcome = {"status": "failed", "error": "sync_execution_failed"}
        with self._lock:
            if self._job is not None and self._job["job_id"] == job_id:
                self._job.update(outcome)
                self._job["finished_at"] = utc_now()
                self._last_finished = time.monotonic()


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
        def setup(self) -> None:
            super().setup()
            self.connection.settimeout(SOCKET_TIMEOUT_SECONDS)

        def log_message(self, format: str, *args: object) -> None:
            # Do not log bearer headers or personal health details.
            pass

        def _send(self, code: int, payload: dict | None = None) -> None:
            data = json.dumps(payload, separators=(",", ":")).encode() if payload is not None else b""
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            if data:
                self.wfile.write(data)

        def _authenticated(self) -> bool:
            supplied = self.headers.get("Authorization", "")
            try:
                valid = hmac.compare_digest(supplied.encode("ascii"), f"Bearer {token}".encode("ascii"))
            except UnicodeEncodeError:
                valid = False
            if valid:
                return True
            self._send(401)
            return False

        def do_GET(self) -> None:
            if self.path == "/healthz":
                self._send(200, {"ok": True})
            elif self.path == "/status":
                if self._authenticated():
                    self._send(200, manager.status())
            elif self.path == "/sync":
                self._send(405)
            else:
                self._send(404)

        def do_POST(self) -> None:
            if self.path != "/sync":
                self._send(404 if self.path != "/status" else 405)
                return
            if not self._authenticated():
                return
            # No payload, mode, shell command or other caller-controlled argument.
            if self.headers.get("Transfer-Encoding") or self.headers.get("Content-Length", "0") != "0":
                self._send(400)
                return
            self._send(200, manager.start())

    return Handler


def main() -> None:
    token = os.environ.get("ZEPPBRIDGE_MCP_AUTH_TOKEN", "")
    if not token.strip():
        raise SystemExit("ZEPPBRIDGE_MCP_AUTH_TOKEN is required")
    host, sep, port = os.environ.get("ZEPPBRIDGE_SYNC_WORKER_ADDR", "127.0.0.1:8081").rpartition(":")
    if not sep or not host:
        raise SystemExit("ZEPPBRIDGE_SYNC_WORKER_ADDR must be host:port")
    server = BoundedHTTPServer((host, int(port)), make_handler(SyncManager(), token))
    print(f"ZeppBridge private sync worker listening on {host}:{port}", flush=True)
    server.serve_forever(poll_interval=0.5)


if __name__ == "__main__":
    main()
