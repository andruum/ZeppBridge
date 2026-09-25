"""Offline protocol, privilege-boundary, and runner tests for the sync MCP."""
import json
import os
import subprocess
import threading
import unittest
import urllib.error
import urllib.request
from unittest.mock import patch

import sync_mcp


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

        manager = sync_mcp.SyncManager(runner=runner)
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
        self.assertEqual(calls[0][0], sync_mcp.SYNC_COMMAND)
        self.assertEqual(calls[0][0][0], "/usr/local/bin/zeppbridge-cli")
        self.assertNotIn("ZEPPBRIDGE_MCP_AUTH_TOKEN", calls[0][1]["env"])
        self.assertNotIn("COOLIFY_API_TOKEN", calls[0][1]["env"])
        self.assertEqual(calls[0][1]["env"]["ZEPPBRIDGE_APP_TOKEN"], "zepp-cloud-token")
        self.assertFalse(calls[0][1]["check"])
        self.assertEqual(calls[0][1]["timeout"], sync_mcp.SYNC_TIMEOUT_SECONDS)

    def test_single_flight_and_cooldown(self):
        waiting = threading.Event()
        proceed = threading.Event()
        calls = []

        def runner(command, **options):
            calls.append(command)
            waiting.set()
            proceed.wait(timeout=2)
            return subprocess.CompletedProcess(command, 0, stdout='{"ok":true,"success":true,"streams":[]}', stderr="")

        manager = sync_mcp.SyncManager(runner=runner)
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
                manager = sync_mcp.SyncManager(runner=lambda command, **kw: subprocess.CompletedProcess(
                    command, code, stdout='{"ok":false,"success":false,"streams":[]}', stderr="ignored"))
                manager.start()
                manager._worker.join(timeout=2)
                self.assertEqual(manager.status()["status"], expected)

    def test_timeout_and_bad_json_do_not_leak_errors(self):
        for runner in (
            lambda command, **kw: (_ for _ in ()).throw(subprocess.TimeoutExpired(command, 900)),
            lambda command, **kw: subprocess.CompletedProcess(command, 0, stdout="not-json", stderr="secret"),
        ):
            manager = sync_mcp.SyncManager(runner=runner)
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
        manager = sync_mcp.SyncManager(runner=runner)
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
        manager = sync_mcp.SyncManager(runner=runner, cooldown_seconds=0)
        manager.start()
        manager._worker.join(timeout=2)
        self.assertEqual(manager.status()["status"], "failed")
        self.assertNotIn("private", json.dumps(manager.status()))
        manager.start()
        manager._worker.join(timeout=2)
        self.assertEqual(manager.status()["status"], "complete")

    def test_thread_start_failure_does_not_leave_running_job(self):
        manager = sync_mcp.SyncManager(runner=lambda *_args, **_kw: None)
        with patch.object(threading.Thread, "start", side_effect=RuntimeError("cannot start")):
            result = manager.start()
        self.assertEqual(result["status"], "failed")
        self.assertEqual(result["error"], "sync_start_failed")
        self.assertIsNone(manager._worker)

    def test_busy_tool_result_is_retryable_error(self):
        result = sync_mcp._tool_result({"status": "busy", "exit_code": 4})
        self.assertTrue(result["isError"])


class HttpProtocolTests(unittest.TestCase):
    def setUp(self):
        self.calls = []

        def runner(command, **options):
            self.calls.append(command)
            return subprocess.CompletedProcess(command, 0, stdout='{"ok":true,"success":true,"recordsWritten":0,"streams":[]}', stderr="")

        self.manager = sync_mcp.SyncManager(runner=runner)
        self.server = sync_mcp.BoundedHTTPServer(("127.0.0.1", 0), sync_mcp.make_handler(self.manager, "same-read-mcp-token"))
        self.server_thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.server_thread.start()
        self.url = f"http://127.0.0.1:{self.server.server_port}/mcp"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.server_thread.join(timeout=2)
        if self.manager._worker:
            self.manager._worker.join(timeout=2)

    def request(self, method, params=None, token="same-read-mcp-token", include_id=True):
        message = {"jsonrpc": "2.0", "method": method, "params": params or {}}
        if include_id:
            message["id"] = 1
        request = urllib.request.Request(self.url, method="POST", data=json.dumps(message).encode(),
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json",
                     "Accept": "application/json, text/event-stream"})
        with urllib.request.urlopen(request, timeout=2) as response:
            return response.status, json.loads(response.read()) if include_id else None

    def test_same_token_authenticates_and_only_fixed_tools_exist(self):
        _, initialized = self.request("initialize", {"protocolVersion": "2025-03-26"})
        self.assertEqual(initialized["result"]["capabilities"], {"tools": {}})
        _, listed = self.request("tools/list")
        self.assertEqual([tool["name"] for tool in listed["result"]["tools"]],
                         ["sync_zepp", "get_sync_status"])
        _, started = self.request("tools/call", {"name": "sync_zepp", "arguments": {}})
        self.assertEqual(started["result"]["structuredContent"]["status"], "running")
        self.manager._worker.join(timeout=2)
        _, status = self.request("tools/call", {"name": "get_sync_status", "arguments": {}})
        self.assertEqual(status["result"]["structuredContent"]["status"], "complete")
        self.assertEqual(self.calls, [sync_mcp.SYNC_COMMAND])

    def test_unauthorized_calls_and_notifications_never_sync(self):
        with self.assertRaises(urllib.error.HTTPError) as caught:
            self.request("tools/call", {"name": "sync_zepp"}, token="wrong-token")
        self.assertEqual(caught.exception.code, 401)
        http_code, _ = self.request("tools/call", {"name": "sync_zepp"}, include_id=False)
        self.assertEqual(http_code, 202)
        self.assertFalse(self.calls)

    def test_no_arguments_or_arbitrary_tool_commands(self):
        for name, arguments in (("sync_zepp", {"mode": "history"}),
                                ("execute_shell", {}), ("get_sync_status", {"cmd": "echo secret"})):
            with self.subTest(name=name, arguments=arguments):
                _, response = self.request("tools/call", {"name": name, "arguments": arguments})
                self.assertEqual(response["error"]["code"], -32602)
        self.assertFalse(self.calls)

    def test_modern_discovery_and_tool_envelopes(self):
        modern = {"_meta": {sync_mcp.MODERN_VERSION_KEY: "2026-07-28"}}
        _, discovered = self.request("server/discover", modern)
        self.assertEqual(discovered["result"]["resultType"], "complete")
        self.assertIn("2026-07-28", discovered["result"]["supportedVersions"])
        _, listed = self.request("tools/list", modern)
        self.assertEqual(listed["result"]["resultType"], "complete")
        self.assertEqual(listed["result"]["_meta"][sync_mcp.MODERN_INFO_KEY]["name"], "zeppbridge-sync")
        _, status = self.request("tools/call", {"name": "get_sync_status", "arguments": {}, **modern})
        self.assertEqual(status["result"]["resultType"], "complete")
        self.assertEqual(status["result"]["structuredContent"], {"status": "idle"})
        self.assertFalse(self.calls)

    def test_protocol_header_and_accept_validation(self):
        data = json.dumps({"jsonrpc": "2.0", "id": 7, "method": "tools/list"}).encode()
        headers = {"Authorization": "Bearer same-read-mcp-token", "Content-Type": "application/json",
                   "MCP-Protocol-Version": "2025-03-26", "Accept": "application/json"}
        with urllib.request.urlopen(urllib.request.Request(self.url, data=data, headers=headers), timeout=2) as response:
            self.assertEqual(response.headers["MCP-Protocol-Version"], "2025-03-26")
            self.assertEqual(json.load(response)["result"]["tools"][0]["name"], "sync_zepp")
        for key, value, expected in (("Accept", "text/plain", 406),
                                     ("MCP-Protocol-Version", "1900-01-01", 400)):
            with self.subTest(key=key):
                with self.assertRaises(urllib.error.HTTPError) as caught:
                    urllib.request.urlopen(urllib.request.Request(self.url, data=data,
                        headers={**headers, key: value}), timeout=2)
                self.assertEqual(caught.exception.code, expected)

    def test_invalid_jsonrpc_and_ids_are_rejected_without_side_effects(self):
        headers = {"Authorization": "Bearer same-read-mcp-token", "Content-Type": "application/json"}
        for body in ({"jsonrpc": "1.0", "id": 1, "method": "tools/call", "params": {"name": "sync_zepp"}},
                     {"jsonrpc": "2.0", "id": {"bad": "id"}, "method": "tools/call", "params": {"name": "sync_zepp"}}):
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(urllib.request.Request(self.url, data=json.dumps(body).encode(),
                    headers=headers), timeout=2)
            self.assertEqual(caught.exception.code, 400)
        self.assertFalse(self.calls)

    def test_connection_limit_rejects_excess_clients(self):
        for _ in range(sync_mcp.MAX_CONNECTIONS):
            self.assertTrue(self.server._slots.acquire(blocking=False))
        try:
            with self.assertRaises(urllib.error.HTTPError) as caught:
                urllib.request.urlopen(self.url.replace("/mcp", "/healthz"), timeout=2)
            self.assertEqual(caught.exception.code, 503)
        finally:
            for _ in range(sync_mcp.MAX_CONNECTIONS):
                self.server._slots.release()

    def test_healthz_and_wrong_path(self):
        with urllib.request.urlopen(self.url.replace("/mcp", "/healthz"), timeout=2) as response:
            self.assertEqual(response.status, 200)
        with self.assertRaises(urllib.error.HTTPError) as caught:
            urllib.request.urlopen(self.url, timeout=2)
        self.assertEqual(caught.exception.code, 405)


if __name__ == "__main__":
    unittest.main()
