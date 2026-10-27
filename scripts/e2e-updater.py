#!/usr/bin/env python3
"""Isolated updater API and restart/rollback checks; no GitHub or installed app changes.
Run after cargo build -p ysyncd: python3 scripts/e2e-updater.py
"""
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.request import Request, build_opener, ProxyHandler

ROOT = Path(__file__).resolve().parents[1]
CLIENT = Path(os.environ.get("CLIENT_BIN", ROOT / "target/debug/ysyncd"))
HTTP = build_opener(ProxyHandler({}))


def wait_for(predicate, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = predicate()
            if value:
                return value
        except (OSError, ValueError, AssertionError):
            pass
        time.sleep(0.1)
    raise AssertionError("Timed out waiting for " + str(predicate))


def scenario(broken=False, api=False, bad_state=False):
    with tempfile.TemporaryDirectory(prefix="ysync update fixture ") as tmp:
        directory = Path(tmp)
        config = directory / "config"
        config.mkdir()
        state = config / "updater.json"
        state.write_text(json.dumps({"auto_check": False, "last_check_at": 0,
                                    "latest_version": "v99.0.0", "release_notes": "fixture notes"}))
        jobfile = config / "update-job.json"
        if api:
            if "--preview" in sys.argv:
                (config / "config.json").write_text(json.dumps({"server_url": "http://127.0.0.1:9", "token": "fixture", "user": "fixture", "device_id": 1, "folders": []}))
            jobfile.write_text(json.dumps({"phase": "downloading" if "--preview" in sys.argv else "restarting", "target_version": "99.0.0",
                "downloaded_bytes": 4194304, "total_bytes": 8388608, "message": "正在下载更新（测试场景）",
                "helper_pid": os.getpid()}))
        if bad_state:
            jobfile.mkdir()
        target = directory / "ysyncd"
        staged = directory / "staged-client"
        backup = directory / "previous-client"
        shutil.copy2(CLIENT, target)
        original_payload = target.read_bytes()
        if broken:
            staged.write_text("#!/bin/sh\nexit 42\n")
            staged.chmod(0o755)
        else:
            shutil.copy2(CLIENT, staged)
        expected_payload = original_payload if broken else staged.read_bytes()
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            address = "127.0.0.1:" + str(sock.getsockname()[1])
        env = {**os.environ, "YSYNC_CONFIG_DIR": str(config), "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"}
        args = ["daemon", "-http", address, "-interval", "60s"]
        log = (directory / "daemon.log").open("w")
        old = subprocess.Popen([str(target)] + args, env=env, stdout=log, stderr=log)
        helper = None
        try:
            info = wait_for(lambda: json.loads((config / "daemon.json").read_text()))

            def request(path, body=None, expected=200, token=True):
                headers = {"Authorization": "Bearer " + info["token"]} if token else {}
                data = json.dumps(body).encode() if body is not None else None
                if data is not None:
                    headers["Content-Type"] = "application/json"
                try:
                    with HTTP.open(Request("http://" + address + path, data=data, headers=headers), timeout=3) as response:
                        status, raw = response.status, response.read()
                except HTTPError as error:
                    status, raw = error.code, error.read()
                assert status == expected, (path, status, raw)
                return json.loads(raw) if raw.startswith(b"{") else raw

            if bad_state:
                for _ in range(2):
                    result = request("/update/check", {}, expected=409)
                    assert "已有更新任务" not in result["error"]
                    assert request("/update/status")["job"]["phase"] == "error"
                print("PASS failed state persistence cannot leave a permanently busy update job", flush=True)
                return
            if api:
                start = time.monotonic()
                result = request("/update/status")
                assert time.monotonic() - start < 1.5, "status must use local cached metadata"
                assert result["auto_check"] is False
                assert result["job"]["phase"] == ("downloading" if "--preview" in sys.argv else "restarting")
                request("/update/status", expected=401, token=False)
                request("/update/check", {}, expected=409)
                request("/update/apply", {}, expected=409)
                request("/update/preferences", {"auto_check": True})
                assert json.loads(state.read_text())["auto_check"] is True
                request("/update/preferences", {"auto_check": False, "skip_version": True})
                saved = json.loads(state.read_text())
                assert saved["auto_check"] is False and saved["ignored_version"] == "v99.0.0"
                print("PASS cached status, authentication, duplicate-job exclusion and persisted preferences", flush=True)
                if "--preview" in sys.argv:
                    preview = Path("/tmp/ysync-updater-preview.json")
                    done = preview.with_suffix(".done")
                    done.unlink(missing_ok=True)
                    preview.write_text(json.dumps({"url": "http://" + address + "/?token=" + info["token"] + "#updates"}))
                    print("Preview ready; create /tmp/ysync-updater-preview.done to finish", flush=True)
                    wait_for(done.exists, timeout=180)
                    preview.unlink(missing_ok=True)
                    done.unlink(missing_ok=True)
                return

            plan = {"target": str(target), "staged": str(staged), "backup": str(backup),
                    "daemon_exe": str(target), "running_daemon_exe": str(target), "daemon_args": args,
                    "daemon_pid": old.pid, "desktop_pids": [], "app": None,
                    "health_url": "http://" + address + "/healthz", "version": "fixture"}
            planfile = directory / "plan.json"
            planfile.write_text(json.dumps(plan))
            jobfile.write_text(json.dumps({"phase": "restarting", "target_version": "fixture",
                "downloaded_bytes": 0, "total_bytes": 0, "message": "test", "helper_pid": None}))
            helper = subprocess.Popen([str(CLIENT), "__update-helper", str(planfile)], env=env,
                                      stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            job = json.loads(jobfile.read_text())
            job["helper_pid"] = helper.pid
            jobfile.write_text(json.dumps(job))
            stdout, stderr = helper.communicate(timeout=30)
            old.wait(timeout=3)
            result = json.loads(jobfile.read_text())
            assert result["phase"] == ("error" if broken else "completed"), (result, stdout, stderr)
            assert helper.returncode == (1 if broken else 0), stderr
            info = wait_for(lambda: json.loads((config / "daemon.json").read_text()))
            assert info["pid"] != old.pid
            assert request("/healthz")["status"] == "ok"
            assert target.read_bytes() == expected_payload, "installed content must match the staged fixture"
            if not broken:
                assert backup.exists(), "working backup must be retained"
            print("PASS " + ("failed launch rolls back and restores old daemon" if broken else
                             "detached helper atomically installs and restarts daemon"), flush=True)
        except Exception:
            print((directory / "daemon.log").read_text()[-4000:])
            helper_log = config / "update.log"
            if helper_log.exists():
                print(helper_log.read_text()[-4000:])
            raise
        finally:
            if helper is not None and helper.poll() is None:
                helper.kill()
                helper.wait()
            current_info = config / "daemon.json"
            if current_info.exists():
                pid = json.loads(current_info.read_text())["pid"]
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
            if old.poll() is None:
                old.terminate()
            old.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    scenario(api=True)
    scenario(bad_state=True)
    scenario()
    scenario(broken=True)
