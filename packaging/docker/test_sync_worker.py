"""Contract tests for the private, non-MCP Zepp sync worker."""
import json
import os
import subprocess
import threading
import unittest
import urllib.error
import urllib.request
from unittest.mock import patch

import sync_worker

from sync_worker import BoundedHTTPServer, MAX_CONNECTIONS, SyncManager, SYNC_COMMAND, make_handler


class SyncManagerTests(unittest.TestCase):
    def test_fixed_incremental_command_and_sanitized_report(self):
        calls = []

        def runner(command, **options):
            calls.append((command, options))
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps({
                "ok": True, "success": True, "recordsWritten": 2,
                "message": "secret-from-upstream-do-not-echo",
                "streams": [{"stream": "sleep", "status": "ok", "recordsWritten": 2,
                             "message": "private-account-id"}],
            }), stderr="secret-stderr")

        manager = sync_worker.SyncManager(runner=runner)
        with patch.dict(os.environ, {"ZEPPBRIDGE_MCP_AUTH_TOKEN": "private-bearer",
                                     "COOLIFY_API_TOKEN": "unrelated-secret",
                                     "ZEPPBRIDGE_APP_TOKEN": "zepp-cloud-token"}):
            started = manager.start()
            manager._worker.join(timeout=2)
        self.assertFalse(manager._worker.is_alive())
        result = manager.status()
        self.assertEqual(started["job_id"], result["job_id"])
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["records_written"], 2)
        self.assertEqual(result["streams"], [{"stream": "sleep", "status": "ok", "records_written": 2}])
        self.assertNotIn("secret", json.dumps(result))
        self.assertEqual(calls[0][0], sync_worker.SYNC_COMMAND)
        self.assertEqual(calls[0][0][0], "/usr/local/bin/zeppbridge-cli")
        self.assertNotIn("ZEPPBRIDGE_MCP_AUTH_TOKEN", calls[0][1]["env"])
        self.assertNotIn("COOLIFY_API_TOKEN", calls[0][1]["env"])
        self.assertEqual(calls[0][1]["env"]["ZEPPBRIDGE_APP_TOKEN"], "zepp-cloud-token")
        self.assertFalse(calls[0][1]["check"])
        self.assertEqual(calls[0][1]["timeout"], sync_worker.SYNC_TIMEOUT_SECONDS)

    def test_single_flight_and_cooldown(self):
        waiting = threading.Event()
        proceed = threading.Event()
        calls = []

        def runner(command, **options):
            calls.append(command)
            waiting.set()
            proceed.wait(timeout=2)
            return subprocess.CompletedProcess(command, 0, stdout='{"ok":true,"success":true,"streams":[]}', stderr="")

        manager = sync_worker.SyncManager(runner=runner)
        first = manager.start()
        self.assertTrue(waiting.wait(timeout=2))
        concurrent = manager.start()
        self.assertEqual(concurrent["job_id"], first["job_id"])
        self.assertTrue(concurrent["coalesced"])
        proceed.set()
        manager._worker.join(timeout=2)
        self.assertEqual(manager.status()["status"], "complete")
        quick = manager.start()
        self.assertEqual(quick["job_id"], first["job_id"])
        self.assertEqual(len(calls), 1)

    def test_cli_lock_contention_and_partial_failure_are_distinct(self):
        for code, expected in ((4, "busy"), (8, "partial"), (5, "failed")):
            with self.subTest(exit_code=code):
                manager = sync_worker.SyncManager(runner=lambda command, **kw: subprocess.CompletedProcess(
                    command, code, stdout='{"ok":false,"success":false,"streams":[]}', stderr="ignored"))
                manager.start()
                manager._worker.join(timeout=2)
                self.assertEqual(manager.status()["status"], expected)

    def test_timeout_and_bad_json_do_not_leak_errors(self):
        for runner in (
            lambda command, **kw: (_ for _ in ()).throw(subprocess.TimeoutExpired(command, 900)),
            lambda command, **kw: subprocess.CompletedProcess(command, 0, stdout="not-json", stderr="secret"),
        ):
            manager = sync_worker.SyncManager(runner=runner)
            manager.start()
            manager._worker.join(timeout=2)
            self.assertEqual(manager.status()["status"], "failed")
            self.assertNotIn("secret", json.dumps(manager.status()))
    def test_simultaneous_callers_start_only_one_sync(self):
        gate = threading.Barrier(9)
        release = threading.Event()
        invocations = []
        results = []
        def runner(command, **options):
            invocations.append(command)
            release.wait(timeout=3)
            return subprocess.CompletedProcess(command, 0, stdout='{"ok":true,"success":true,"streams":[]}', stderr="")
        manager = sync_worker.SyncManager(runner=runner)
        def call():
            gate.wait(timeout=3)
            results.append(manager.start())
        threads = [threading.Thread(target=call) for _ in range(8)]
        for thread in threads:
            thread.start()
        gate.wait(timeout=3)
        for thread in threads:
            thread.join(timeout=3)
        self.assertTrue(all(not thread.is_alive() for thread in threads))
        self.assertEqual(len(results), 8)
        self.assertEqual(len({result["job_id"] for result in results}), 1)
        self.assertEqual(len(invocations), 1)
        release.set()
        manager._worker.join(timeout=3)

    def test_unexpected_runner_exception_recovers_for_next_request(self):
        calls = []
        def runner(command, **options):
            calls.append(command)
            if len(calls) == 1:
                raise RuntimeError("private internal failure")
            return subprocess.CompletedProcess(command, 0, stdout='{"ok":true,"success":true,"streams":[]}', stderr="")
        manager = sync_worker.SyncManager(runner=runner, cooldown_seconds=0)
        manager.start()
        manager._worker.join(timeout=2)
        self.assertEqual(manager.status()["status"], "failed")
        self.assertNotIn("private", json.dumps(manager.status()))
        manager.start()
        manager._worker.join(timeout=2)
        self.assertEqual(manager.status()["status"], "complete")

    def test_thread_start_failure_does_not_leave_running_job(self):
        manager = sync_worker.SyncManager(runner=lambda *_args, **_kw: None)
        with patch.object(threading.Thread, "start", side_effect=RuntimeError("cannot start")):
            result = manager.start()
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error"], "sync_start_failed")
        self.assertIsNone(manager._worker)


class WorkerHTTPTests(unittest.TestCase):
    def setUp(self):
        self.calls = []
        def runner(command, **options):
            self.calls.append(command)
            return subprocess.CompletedProcess(command, 0, stdout=json.dumps({
                "ok": True, "success": True, "recordsWritten": 1,
                "message": "private-data-must-not-leak",
                "streams": [{"stream": "sleep", "status": "success", "recordsWritten": 1}],
            }), stderr="private-error")
        self.manager = SyncManager(runner=runner)
        self.server = BoundedHTTPServer(("127.0.0.1", 0), make_handler(self.manager, "shared-mcp-token"))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base = f"http://127.0.0.1:{self.server.server_port}"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        if self.manager._worker:
            self.manager._worker.join(timeout=2)

    def fetch(self, path, method="GET", token="shared-mcp-token", body=None):
        headers = {"Authorization": f"Bearer {token}"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self.base + path, headers=headers, data=body, method=method)
        with urllib.request.urlopen(request, timeout=3) as response:
            return response.status, json.load(response)

    def test_fixed_sync_and_status_are_private_non_mcp_json(self):
        code, result = self.fetch("/sync", method="POST", body=b"")
        self.assertEqual(code, 200)
        self.assertIn(result["status"], ("running", "complete"))
        self.manager._worker.join(timeout=2)
        self.assertEqual(self.calls, [SYNC_COMMAND])
        _, result = self.fetch("/status")
        self.assertEqual(result["status"], "complete")
        self.assertEqual(result["records_written"], 1)
        self.assertNotIn("private-", json.dumps(result))
        self.assertNotIn("jsonrpc", result)
        with self.assertRaises(urllib.error.HTTPError) as error:
            self.fetch("/mcp", method="POST", body=b"{}")
        self.assertEqual(error.exception.code, 404)

    def test_unauthenticated_calls_never_start_a_sync(self):
        for path, method in (("/sync", "POST"), ("/status", "GET")):
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.fetch(path, method=method, token="incorrect", body=b"" if method == "POST" else None)
            self.assertEqual(error.exception.code, 401)
        self.assertFalse(self.calls)

    def test_non_ascii_bearer_is_rejected_without_server_exception(self):
        with self.assertRaises(urllib.error.HTTPError) as error:
            self.fetch("/status", token="ñ")
        self.assertEqual(error.exception.code, 401)
        self.assertFalse(self.calls)

    def test_worker_refuses_any_caller_arguments(self):
        for body in (b"{}", b'{"mode":"full"}', b"not-json"):
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.fetch("/sync", method="POST", body=body)
            self.assertEqual(error.exception.code, 400)
        self.assertFalse(self.calls)

    def test_connection_limit_rejects_excess_clients(self):
        for _ in range(MAX_CONNECTIONS):
            self.assertTrue(self.server._slots.acquire(blocking=False))
        try:
            with self.assertRaises(urllib.error.HTTPError) as error:
                self.fetch("/healthz")
            self.assertEqual(error.exception.code, 503)
        finally:
            for _ in range(MAX_CONNECTIONS):
                self.server._slots.release()

    def test_healthz_is_public_but_status_is_not(self):
        code, result = self.fetch("/healthz", token="incorrect")
        self.assertEqual(code, 200)
        self.assertTrue(result["ok"])
        with self.assertRaises(urllib.error.HTTPError) as error:
            self.fetch("/sync", token="incorrect")
        self.assertIn(error.exception.code, (404, 405))