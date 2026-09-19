#!/usr/bin/env python3
"""Wipe-and-restore: seed three shaped databases, snapshot every row plus
export files OUTSIDE the data dir, rm -rf the data dir (simulated total
loss), re-init with a FRESH owner key, re-import everything, prove every
row byte-exact and the registry exact. Dummy data, local only.

Usage: scripts/wipe_restore.py <path-to-dg-binary>
Exit nonzero unless all dumps match exactly.
"""
import base64
import json
import os
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import urllib.request

BIN = sys.argv[1]
DBS = ["w1", "w2", "w3"]
TABLES = {
    "w1": [("kv", "id, k, n")],
    "w2": [("parent", "id, name"), ("child", "id, pid, tag")],
    "w3": [("misc", "id, v")],
}


def start_server(data_dir):
    proc = subprocess.Popen(
        [BIN, "serve", "--listen", "127.0.0.1:0", "--data-dir", data_dir],
        stderr=subprocess.PIPE, text=True,
    )
    lines = queue.Queue()
    def drain():
        for line in proc.stderr:
            lines.put(line)
    threading.Thread(target=drain, daemon=True).start()
    addr = None
    for _ in range(1000):
        try:
            line = lines.get(timeout=30)
        except queue.Empty:
            break
        try:
            addr = json.loads(line)["fields"].get("address")
        except (ValueError, KeyError, TypeError):
            continue
        if addr:
            break
    assert addr, "server never logged its listen address"
    print(f"serving on {addr} (pid {proc.pid})", flush=True)
    return proc, addr


class Api:
    def __init__(self, addr, key):
        self.addr = addr
        self.key = key

    def req(self, method, path, body=None, idem=None):
        headers = {"Authorization": f"Bearer {self.key}",
                   "Content-Type": "application/json"}
        if idem:
            headers["Idempotency-Key"] = idem
        r = urllib.request.Request(
            f"http://{self.addr}{path}",
            data=json.dumps(body or {}).encode(), method=method,
            headers=headers)
        try:
            with urllib.request.urlopen(r, timeout=60) as x:
                return x.status, json.loads(x.read() or b"null")
        except urllib.error.HTTPError as e:
            raise AssertionError(f"{method} {path} -> {e.code}: {e.read()[:300]}")


def seed(api):
    for db in DBS:
        s, _ = api.req("POST", "/v1/databases", {"name": db})
        assert s == 200, db
    api.req("POST", "/v1/databases/w1/query",
            {"sql": "CREATE TABLE kv(id INTEGER PRIMARY KEY, k TEXT, n REAL)"}, "ddl-w1")
    stmts = [{"sql": "INSERT INTO kv(k, n) VALUES ('k-%d', %f)" % (i, i * 1.5)}
             for i in range(200)]
    for n in range(0, len(stmts), 32):
        s, b = api.req("POST", "/v1/databases/w1/batch",
                       {"statements": stmts[n:n + 32], "atomic": True}, f"seed-w1-{n}")
        assert s == 200, b
    api.req("POST", "/v1/databases/w2/query",
            {"sql": "CREATE TABLE parent(id INTEGER PRIMARY KEY, name TEXT)"}, "ddl-w2a")
    api.req("POST", "/v1/databases/w2/query",
            {"sql": "CREATE TABLE child(id INTEGER PRIMARY KEY, pid INTEGER REFERENCES parent(id), tag TEXT)"}, "ddl-w2b")
    stmts = [{"sql": "INSERT INTO parent(name) VALUES ('p-%d')" % i} for i in range(100)]
    for n in range(0, len(stmts), 32):
        s, b = api.req("POST", "/v1/databases/w2/batch",
                       {"statements": stmts[n:n + 32], "atomic": True}, f"seed-w2p-{n}")
        assert s == 200, b
    stmts = [{"sql": "INSERT INTO child(pid, tag) VALUES (%d, 'c-%d')" % (i % 100 + 1, i)}
             for i in range(300)]
    for n in range(0, len(stmts), 32):
        s, b = api.req("POST", "/v1/databases/w2/batch",
                       {"statements": stmts[n:n + 32], "atomic": True}, f"seed-w2c-{n}")
        assert s == 200, b
    api.req("POST", "/v1/databases/w3/query",
            {"sql": "CREATE TABLE misc(id INTEGER PRIMARY KEY, v TEXT)"}, "ddl-w3")
    edge = [None, "", "unicode- héllo-日本語", "quote's \"dq\"", "line\nbreak\ttab",
            "x" * 5000]
    for i, v in enumerate(edge * 8 + ["tail-%d" % i for i in range(2)]):
        s, b = api.req("POST", "/v1/databases/w3/query",
                       {"sql": "INSERT INTO misc(v) VALUES (?)", "args": [v]}, f"seed-w3-{i}")
        assert s == 200, b
    print("seeded: w1=200 kv, w2=100 parent + 300 child, w3=50 misc (NULL/unicode/5KB edges)", flush=True)


def dump(api, db, table, cols):
    s, b = api.req("POST", f"/v1/databases/{db}/query",
                   {"sql": f"SELECT {cols} FROM {table} ORDER BY id"})
    assert s == 200, b
    return b["rows"]


def main():
    tmp = tempfile.mkdtemp(prefix="wipe-restore-")
    data = os.path.join(tmp, "data")
    backup = os.path.join(tmp, "backup")
    os.makedirs(data)
    os.makedirs(backup)
    out = subprocess.run([BIN, "init", "--data-dir", data],
                         capture_output=True, text=True, check=True)
    key = json.loads(out.stdout)["owner_key"]
    print("initialized original data dir", flush=True)

    proc, addr = start_server(data)
    api = Api(addr, key)
    seed(api)

    # Snapshot every row + export files (outside the data dir).
    dumps = {}
    for db, tables in TABLES.items():
        for table, cols in tables:
            rows = dump(api, db, table, cols)
            dumps[f"{db}.{table}"] = rows
            print(f"pre-wipe {db}.{table}: {len(rows)} rows", flush=True)
    for db in DBS:
        s, b = api.req("POST", f"/v1/databases/{db}/export", {})
        assert s == 200, b
        raw = base64.b64decode(b["data_base64"])
        with open(os.path.join(backup, f"{db}.sqlite"), "wb") as f:
            f.write(raw)
        print(f"export {db}: {b['size']} bytes sha256={b['sha256'][:16]}…", flush=True)

    # Total loss.
    proc.terminate()
    proc.wait()
    shutil.rmtree(data)
    assert not os.path.exists(data), "wipe failed"
    print("data dir wiped", flush=True)

    # Fresh world, fresh owner key.
    os.makedirs(data)
    out = subprocess.run([BIN, "init", "--data-dir", data],
                         capture_output=True, text=True, check=True)
    key2 = json.loads(out.stdout)["owner_key"]
    assert key2 != key, "re-init reused the owner key"
    print("re-initialized with a fresh owner key", flush=True)
    proc, addr = start_server(data)
    api = Api(addr, key2)

    for db in DBS:
        with open(os.path.join(backup, f"{db}.sqlite"), "rb") as f:
            payload = base64.b64encode(f.read()).decode()
        s, b = api.req("POST", f"/v1/databases/{db}/import",
                       {"name": db, "data_base64": payload})
        assert s == 200, (db, b)
        print(f"imported {db}", flush=True)

    s, b = api.req("GET", "/v1/databases", {})
    assert s == 200 and sorted(b["databases"]) == DBS, b
    for db, tables in TABLES.items():
        for table, cols in tables:
            rows = dump(api, db, table, cols)
            assert rows == dumps[f"{db}.{table}"], \
                f"{db}.{table}: {len(rows)} vs {len(dumps[f'{db}.{table}'])} rows"
            print(f"post-restore {db}.{table}: {len(rows)} rows byte-exact", flush=True)

    proc.terminate()
    proc.wait()
    print("WIPE-AND-RESTORE PASS", flush=True)


main()
