#!/usr/bin/env python3
"""Remote project selection against temporary Rust server/daemon instances.

Run: cargo build -p ysyncd -p ysync-server-rs
     python3 scripts/e2e-project-selection.py
Only the Python standard library is required. No real account/config is used.
"""
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlparse
from urllib.request import Request, ProxyHandler, build_opener

ROOT = Path(__file__).resolve().parents[1]
CLIENT = Path(os.environ.get("CLIENT_BIN", ROOT / "target/debug/ysyncd"))
SERVER = Path(os.environ.get("SERVER_BIN", ROOT / "target/debug/ysync-server-rs"))
HTTP = build_opener(ProxyHandler({}))


def request(base, path, body=None, token="", method=None, expected=200):
    headers = {"Authorization": "Bearer " + token}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = Request(base + path, data=data, headers=headers, method=method)
    try:
        with HTTP.open(req, timeout=12) as response:
            status, raw = response.status, response.read()
    except HTTPError as error:
        status, raw = error.code, error.read()
    assert status == expected, (path, status, raw.decode(errors="replace"))
    return json.loads(raw) if status == 200 else raw


def wait_for(predicate, timeout=12):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, ValueError, AssertionError):
            pass
        time.sleep(0.1)
    raise AssertionError("timed out waiting for " + str(predicate))


def verify_legacy_and_invalid_pages():
    def node(node_id, path):
        return {"id": node_id, "path": path, "name": path.rsplit("/", 1)[-1], "type": "dir"}

    cases = [
        ("legacy-unrelated-pages", [node(i, f"other/d{i}") for i in range(1, 10001)]
         + [node(10001, "target"), node(10002, "target/sub")], [0, 5000, 10000], False, ""),
        ("root-at-page-boundary", [node(i, f"other/d{i}") for i in range(1, 5000)]
         + [node(5000, "target"), node(5001, "target/sub")], [0, 5000], False, ""),
        ("empty-final-page", [], [0], False, ""),
        ("invalid-empty-page", [], [0], True, "节点分页返回空页"),
        ("nonadvancing-page", [node(0, "other/stuck")], [0], True, "节点分页游标未推进"),
    ]
    for name, nodes, expected_after, force_more, error in cases:
        cursors = []

        class Mock(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                parsed = urlparse(self.path)
                query = parse_qs(parsed.query)
                if parsed.path == "/api/v1/projects":
                    self.send_error(404)  # Old servers only have /nodes.
                    return
                if parsed.path == "/api/v1/nodes" and "limit" not in query:
                    result = {"nodes": [node(10001, "target")]}
                elif parsed.path == "/api/v1/nodes":
                    after = int(query["after"][0])
                    cursors.append(after)
                    page = nodes if force_more else [n for n in nodes if n["id"] > after]
                    result = {"nodes": page[:5000], "has_more": force_more or len(page) > 5000}
                    if len(cursors) > 5:
                        self.send_error(500)
                        return
                elif parsed.path == "/api/v1/sync/head":
                    result = {"cursor": 12345}
                else:
                    self.send_error(500)
                    return
                raw = json.dumps(result).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

        mock = ThreadingHTTPServer(("127.0.0.1", 0), Mock)
        threading.Thread(target=mock.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory(prefix="ysync-paging-") as tmp:
                config = Path(tmp) / "config.json"
                config.write_text(json.dumps({"server_url": f"http://127.0.0.1:{mock.server_port}",
                    "token": "test", "device_id": 1, "folders": [{"name": "target",
                    "local_path": str(Path(tmp) / "target"), "root_node_id": 0, "cursor": 0}]}))
                result = subprocess.run([str(CLIENT), "sync"], env={**os.environ,
                    "YSYNC_CONFIG_DIR": tmp, "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"},
                    capture_output=True, text=True, timeout=10)
                assert cursors == expected_after, (name, cursors)
                assert (result.returncode != 0) == bool(error), (name, result.stderr)
                if error:
                    assert error in result.stderr, result.stderr
                else:
                    assert json.loads(config.read_text())["folders"][0]["cursor"] == 12345
                    assert not (Path(tmp) / "target/other").exists()
                    if nodes:
                        assert (Path(tmp) / "target/sub").is_dir()
                print("PASS " + name, flush=True)
        finally:
            mock.shutdown()
            mock.server_close()


def main():
    processes = []
    release = threading.Event()
    entered = threading.Event()
    proxy = None
    with tempfile.TemporaryDirectory(prefix="ysync-selection-") as tmp:
        work = Path(tmp)
        data = work / "server"
        cfg = work / "config"
        cfg.mkdir()
        try:
            for user in ("alice", "bob"):
                subprocess.run([str(SERVER), "adduser", user, "--password", "test-password"],
                               env={**os.environ, "YSYNC_DATA": str(data)}, check=True,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            # A large unrelated tree reproduces the original first-sync pagination risk.
            with sqlite3.connect(data / "y-sync.db") as db:
                alice = db.execute("SELECT id FROM users WHERE name='alice'").fetchone()[0]
                bob = db.execute("SELECT id FROM users WHERE name='bob'").fetchone()[0]
                rows = [(1, alice, 0, "other", "dir", "other")]
                rows += [(i, alice, 1, f"d{i}", "dir", f"other/d{i}") for i in range(2, 10002)]
                rows += [(10002, alice, 0, "alpha", "dir", "alpha"),
                         (10003, alice, 0, "beta", "dir", "beta"),
                         (10004, bob, 0, "private", "dir", "private")]
                db.executemany("INSERT INTO nodes(id,user_id,parent_id,name,type,path) VALUES(?,?,?,?,?,?)", rows)
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            server_url = f"http://127.0.0.1:{port}"
            server_log = (work / "server.log").open("w")
            processes.append(subprocess.Popen([str(SERVER), "serve", "-addr", f"127.0.0.1:{port}", "-data", str(data)], stdout=server_log, stderr=server_log))
            wait_for(lambda: request(server_url, "/healthz"))
            login = request(server_url, "/api/v1/auth/login", {"user": "alice", "password": "test-password", "device_name": "seed"})
            token = login["token"]
            content = b"remote content\n"
            digest = hashlib.sha256(content).hexdigest()
            with HTTP.open(Request(server_url + "/api/v1/content", data=content,
                                   headers={"Authorization": "Bearer " + token}, method="PUT"), timeout=10) as response:
                assert response.status == 200
            for root_id in (10002, 10003):
                result = request(server_url, "/api/v1/ops", [{"op": "put", "parent_id": root_id,
                    "name": "remote.txt", "content_hash": digest, "size": len(content), "mtime": 1700000000000}], token)
                assert result["results"][0]["ok"], result
            projects = request(server_url, "/api/v1/projects", token=token)["projects"]
            assert {p["name"] for p in projects} == {"other", "alpha", "beta"}
            scoped = request(server_url, "/api/v1/nodes?root=10002&after=0&limit=5000", token=token)
            assert {n["path"] for n in scoped["nodes"]} == {"alpha", "alpha/remote.txt"}
            assert not scoped["has_more"]
            request(server_url, "/api/v1/projects", expected=401)
            # The HTTP layer must reject a root owned by another user.
            request(server_url, "/api/v1/nodes?root=10004&after=0&limit=5000", token=token, expected=500)
            print("PASS authenticated project discovery, account isolation and scoped pagination", flush=True)

            class Proxy(BaseHTTPRequestHandler):
                def log_message(self, *args):
                    pass

                def forward(self):
                    if self.headers.get("Upgrade"):
                        self.send_error(400)
                        return
                    parsed = urlparse(self.path)
                    if parsed.path == "/api/v1/nodes" and parse_qs(parsed.query).get("root") == ["10002"]:
                        entered.set()
                        release.wait(10)
                    body = self.rfile.read(int(self.headers.get("Content-Length", 0))) or None
                    headers = {k: v for k, v in self.headers.items() if k.lower() in ("authorization", "content-type")}
                    req = Request(server_url + self.path, data=body, headers=headers, method=self.command)
                    try:
                        response = HTTP.open(req, timeout=12)
                    except HTTPError as error:
                        response = error
                    with response:
                        raw = response.read()
                        self.send_response(response.code)
                        self.send_header("Content-Type", response.headers.get("Content-Type", "application/json"))
                        self.send_header("Content-Length", str(len(raw)))
                        self.end_headers()
                        self.wfile.write(raw)

                do_GET = do_POST = do_PUT = forward

            proxy = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
            proxy.daemon_threads = True
            threading.Thread(target=proxy.serve_forever, daemon=True).start()
            daemon_log = (work / "daemon.log").open("w")
            processes.append(subprocess.Popen([str(CLIENT), "daemon", "-http", "127.0.0.1:0", "-interval", "300ms"],
                env={**os.environ, "YSYNC_CONFIG_DIR": str(cfg), "NO_PROXY": "127.0.0.1", "no_proxy": "127.0.0.1"},
                stdout=daemon_log, stderr=daemon_log))
            info = wait_for(lambda: json.loads((cfg / "daemon.json").read_text()))
            local_url = "http://" + info["addr"]
            local_token = info["token"]

            def local(path, body=None, expected=200):
                return request(local_url, path, body, local_token, expected=expected)

            local("/setup", {"server_url": f"http://127.0.0.1:{proxy.server_port}", "user": "alice",
                "password": "test-password", "device_name": "selected-client"})
            assert len(local("/remote-folders")["projects"]) == 3
            request(local_url, "/remote-folders", expected=401)
            target_a = work / "downloads" / "alpha"
            target_a.mkdir(parents=True)
            (target_a / "local.txt").write_text("existing local content\n")
            local("/remote-folders/subscribe", {"root_node_id": 10004, "local_path": str(work / "private")}, expected=400)
            local("/remote-folders/subscribe", {"root_node_id": 10002, "local_path": "relative/path"}, expected=400)
            local("/remote-folders/subscribe", {"root_node_id": 10002, "local_path": str(target_a)})
            assert entered.wait(5), "alpha sync should reach the delayed nodes request"
            local("/remote-folders/subscribe", {"root_node_id": 10002, "local_path": str(work / "duplicate")}, expected=400)
            local("/remote-folders/subscribe", {"root_node_id": 10003, "local_path": str(target_a)}, expected=400)
            local("/remote-folders/subscribe", {"root_node_id": 10003, "local_path": str(target_a / "nested")}, expected=400)
            target_b = work / "downloads" / "beta"
            local("/remote-folders/subscribe", {"root_node_id": 10003, "local_path": str(target_b)})
            wait_for(lambda: (target_b / "remote.txt").exists(), timeout=5)
            assert not (target_a / "remote.txt").exists(), "alpha is deliberately blocked"
            statuses = {s["name"]: s for s in local("/status")["folders"]}
            assert statuses["alpha"]["syncing"]
            print("PASS duplicate/path overlap rejection; beta completes while alpha is blocked", flush=True)
            release.set()
            wait_for(lambda: (target_a / "remote.txt").exists())
            wait_for(lambda: all(s["cursor"] > 0 and not s["syncing"] for s in local("/status")["folders"]))
            assert (target_a / "remote.txt").read_bytes() == content
            assert (target_a / "local.txt").read_text() == "existing local content\n"
            nodes = request(server_url, "/api/v1/nodes?root=10002&after=0&limit=5000", token=token)["nodes"]
            assert "alpha/local.txt" in {n["path"] for n in nodes}, "existing local file must upload"
            configured = json.loads((cfg / "config.json").read_text())["folders"]
            assert {f["root_node_id"] for f in configured} == {10002, 10003}
            assert all(f["cursor"] > 0 for f in configured)
            assert not (work / "downloads" / "other").exists()
            with sqlite3.connect(target_a / ".y-sync/state.db") as db:
                assert int(db.execute("SELECT value FROM meta WHERE key='cursor'").fetchone()[0]) > 0
            assert all(p["subscribed"] for p in local("/remote-folders")["projects"] if p["name"] in ("alpha", "beta"))
            local("/remove", {"name": "beta"})
            assert (target_b / "remote.txt").exists()
            assert any(p["name"] == "beta" for p in request(server_url, "/api/v1/projects", token=token)["projects"])
            print("PASS first sync, bidirectional merge, root ID/cursor persistence and non-destructive removal", flush=True)
            if "--preview" in sys.argv:
                preview = Path("/tmp/ysync-selection-preview.json")
                done = preview.with_suffix(".done")
                done.unlink(missing_ok=True)
                preview.write_text(json.dumps({"url": local_url + "/?token=" + local_token}))
                print("Preview ready; create /tmp/ysync-selection-preview.done to finish", flush=True)
                wait_for(done.exists, timeout=180)
                preview.unlink(missing_ok=True)
                done.unlink(missing_ok=True)
        except Exception:
            for log in work.glob("*.log"):
                print(log.name + ":\n" + log.read_text()[-6000:])
            raise
        finally:
            release.set()
            for process in reversed(processes):
                process.terminate()
            for process in reversed(processes):
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            if proxy:
                proxy.shutdown()
                proxy.server_close()


if __name__ == "__main__":
    verify_legacy_and_invalid_pages()
    main()
