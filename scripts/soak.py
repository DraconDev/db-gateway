#!/usr/bin/env python3
"""Steady read/write soak against a real `dg serve` (dummy data).

Usage: soak.py --dg-bin ./target/debug/dg --seconds 1800 --out /tmp/soak.log

Spawns `dg init` + `dg serve` on a temp data dir, then drives a mixed
workload (keyed single-row INSERTs, point SELECTs, range SELECTs,
occasional 8-statement batches and exports) until the clock runs out.
Every 60s it samples the server's VmRSS. At the end it asserts:

  * zero request errors (any non-200, any exception),
  * server process still alive,
  * final row count == rows inserted (no lost or phantom writes),
  * p99 request latency < 1000 ms,
  * VmRSS growth < 50 MiB from the first sample.

Prints a one-line JSON summary to stdout and appends per-minute
`{"t", "rss_kb", "reqs", "errs", "p50_ms", "p99_ms"}` samples to --out.
Exit nonzero on any failed assertion.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

READINESS_TIMEOUT = 60


def call(base, key, method, path, body=None, timeout=30):
    req = urllib.request.Request(
        base + path,
        data=json.dumps(body or {}).encode(),
        method=method,
        headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            payload = json.loads(r.read().decode())
            return (time.monotonic() - t0) * 1000.0, r.status, payload
    except urllib.error.HTTPError as e:
        try:
            payload = json.loads(e.read().decode())
        except Exception:
            payload = {}
        return (time.monotonic() - t0) * 1000.0, e.code, payload


def vmrss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError("no VmRSS line")


def post_keyed(base, key, path, body, idem):
    req = urllib.request.Request(
        base + path,
        data=json.dumps(body).encode(),
        method="POST",
        headers={
            "Authorization": f"Bearer {key}",
            "Content-Type": "application/json",
            "Idempotency-Key": idem,
        },
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return (time.monotonic() - t0) * 1000.0, r.status, json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        try:
            payload = json.loads(e.read().decode())
        except Exception:
            payload = {}
        return (time.monotonic() - t0) * 1000.0, e.code, payload


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dg-bin", required=True)
    ap.add_argument("--seconds", type=int, default=1800)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    readiness_timeout = float(
        os.environ.get("SOAK_READINESS_TIMEOUT", str(READINESS_TIMEOUT))
    )
    if readiness_timeout <= 0:
        ap.error("--readiness timeout must be positive")

    tmp = tempfile.mkdtemp(prefix="soak-")
    init = subprocess.run(
        [args.dg_bin, "init", "--data-dir", tmp], capture_output=True, text=True
    )
    assert init.returncode == 0, init.stderr
    owner = json.loads(init.stdout)["owner_key"]

    server = subprocess.Popen(
        [args.dg_bin, "serve", "--listen", "127.0.0.1:0", "--data-dir", tmp],
        stderr=subprocess.PIPE,
        text=True,
    )
    stderr_lines = queue.Queue()

    def drain_stderr():
        for line in iter(server.stderr.readline, ""):
            stderr_lines.put(line)

    stderr_thread = threading.Thread(target=drain_stderr, daemon=True)
    stderr_thread.start()
    try:
        base = None
        deadline = time.monotonic() + readiness_timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("server never logged its listen address")
            if server.poll() is not None:
                raise AssertionError(
                    f"server exited during readiness rc={server.returncode}"
                )
            try:
                line = stderr_lines.get(timeout=min(remaining, 0.25))
            except queue.Empty:
                continue
            try:
                addr = json.loads(line)["fields"].get("address")
            except Exception:
                addr = None
            if addr:
                base = f"http://{addr}"
                break
        assert base, "server never logged its listen address"

        lat, st, _ = call(base, owner, "POST", "/v1/databases", {"name": "soak"})
        assert st == 200, st
        DBS = [f"soak_{c}" for c in "abcd"]
        for db in DBS:
            lat, st, _ = call(base, owner, "POST", "/v1/databases", {"name": db})
            assert st == 200, (db, st)
            lat, st, _ = call(
                base,
                owner,
                "POST",
                f"/v1/databases/{db}/query",
                {"sql": "CREATE TABLE events(id INTEGER PRIMARY KEY, tag TEXT, score REAL, payload BLOB)"},
            )
            assert st == 200, (db, st)

        latencies, errors = [], 0
        admitted = {db: 0 for db in DBS}
        seq = 0
        end = time.monotonic() + args.seconds
        minute = 0
        minute_start = time.monotonic()
        exported_once = False
        first_rss = vmrss_kb(server.pid)
        with open(args.out, "a") as log:
            while time.monotonic() < end:
                # One mixed iteration, round-robin across four databases:
                # unkeyed write, point read, aggregate read; every 25th adds
                # an 8-row batch. Every minute adds one keyed write per DB,
                # one scratch-DB create, and one scratch-DB delete (tombstone)
                # of the previous minute's scratch DB. No retries anywhere:
                # any non-200 is an error. Bulk writes are unkeyed
                # (documented, allowed); keyed writes stay far under the
                # 256-keys-per-principal-per-database cap.
                # One mixed iteration: write, point read, range read;
                # every 25th adds a batch, every 100th an export.
                seq += 1
                db = DBS[seq % 4]
                lat, st, _ = call(
                    base,
                    owner,
                    "POST",
                    f"/v1/databases/{db}/query",
                    {
                        "sql": "INSERT INTO events(tag, score, payload) VALUES (?, ?, randomblob(64))",
                        "args": [f"soak-{seq}", seq * 0.5],
                    },
                )
                latencies.append(lat)
                if st == 200:
                    admitted[db] += 1
                else:
                    errors += 1

                lat, st, _ = call(
                    base,
                    owner,
                    "POST",
                    f"/v1/databases/{db}/query",
                    {"sql": "SELECT tag, score FROM events WHERE id = ?", "args": [seq]},
                )
                latencies.append(lat)
                errors += st != 200

                lat, st, _ = call(
                    base,
                    owner,
                    "POST",
                    f"/v1/databases/{db}/query",
                    {"sql": "SELECT count(*), max(id) FROM events"},
                )
                latencies.append(lat)
                errors += st != 200

                if seq % 25 == 0:
                    lat, st, _ = call(
                        base,
                        owner,
                        "POST",
                        f"/v1/databases/{db}/batch",
                        {
                            "statements": [
                                {"sql": f"INSERT INTO events(tag) VALUES ('b-{seq}-{k}')"}
                                for k in range(8)
                            ],
                            "atomic": True,
                        },
                    )
                    latencies.append(lat)
                    if st == 200:
                        admitted[db] += 8
                    else:
                        errors += 1

                if time.monotonic() - minute_start >= 60 or time.monotonic() >= end:
                    minute += 1
                    minute_start = time.monotonic()
                    # One keyed write per database per minute: unique keys,
                    # ~30 per DB over the run, far under the 256 cap.
                    for db in DBS:
                        lat, st, _ = post_keyed(
                            base,
                            owner,
                            f"/v1/databases/{db}/query",
                            {"sql": "INSERT INTO events(tag) VALUES (?)",
                             "args": [f"keyed-{db}-{minute}"]},
                            f"soak-key-{db}-{minute}",
                        )
                        latencies.append(lat)
                        if st == 200:
                            admitted[db] += 1
                        else:
                            errors += 1
                    if not exported_once:
                        exported_once = True
                        lat, st, _ = call(
                            base, owner, "POST", "/v1/databases/soak_a/export", {}
                        )
                        latencies.append(lat)
                        errors += st != 200
                    # Lifecycle churn: create this minute's scratch DB with
                    # one row, tombstone the previous minute's scratch DB.
                    cdb = f"churn_m{minute}"
                    lat, st, _ = call(base, owner, "POST", "/v1/databases", {"name": cdb})
                    latencies.append(lat)
                    errors += st != 200
                    lat, st, _ = call(
                        base, owner, "POST", f"/v1/databases/{cdb}/query",
                        {"sql": "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)"},
                    )
                    latencies.append(lat)
                    errors += st != 200
                    if minute > 1:
                        lat, st, _ = call(base, owner, "DELETE", f"/v1/databases/churn_m{minute - 1}", {})
                        latencies.append(lat)
                        errors += st != 200
                    s = sorted(latencies)
                    sample = {
                        "minute": minute,
                        "rss_kb": vmrss_kb(server.pid),
                        "reqs": len(latencies),
                        "errs": errors,
                        "p50_ms": round(s[len(s) // 2], 2),
                        "p99_ms": round(s[min(len(s) - 1, int(len(s) * 0.99))], 2),
                    }
                    log.write(json.dumps(sample) + "\n")
                    log.flush()

        assert server.poll() is None, f"server died mid-soak rc={server.returncode}"
        for db in DBS:
            lat, st, body = call(
                base,
                owner,
                "POST",
                f"/v1/databases/{db}/query",
                {"sql": "SELECT count(*) FROM events"},
            )
            assert st == 200, (db, body)
            assert body["rows"] == [[admitted[db]]], \
                f"{db} count {body['rows']} != admitted {admitted[db]}"

        inserted = sum(admitted.values())

        # Lifecycle registry proof: the latest scratch DB is live, the one
        # before it is tombstoned (absent), soak DBs untouched by churn.
        lat, st, body = call(base, owner, "GET", "/v1/databases", {})
        assert st == 200, body
        names = body["databases"]
        assert f"churn_m{minute}" in names, names
        if minute > 1:
            assert f"churn_m{minute - 1}" not in names, names
        for db in DBS + ["soak"]:
            assert db in names, names

        s = sorted(latencies)
        p99 = s[min(len(s) - 1, int(len(s) * 0.99))]
        p50 = s[len(s) // 2]
        rss_end = vmrss_kb(server.pid)
        summary = {
            "seconds": args.seconds,
            "reqs": len(latencies),
            "errors": errors,
            "inserted": inserted,
            "p50_ms": round(p50, 2),
            "p99_ms": round(p99, 2),
            "rss_start_kb": first_rss,
            "rss_end_kb": rss_end,
        }
        print(json.dumps(summary), flush=True)
        assert errors == 0, f"{errors} request errors"
        assert p99 < 1000.0, f"p99 {p99:.1f}ms"
        assert rss_end - first_rss < 50 * 1024, f"rss grew {rss_end - first_rss} KiB"
        print("SOAK PASS", flush=True)
    finally:
        server.kill()
        server.wait()


if __name__ == "__main__":
    sys.exit(main())
