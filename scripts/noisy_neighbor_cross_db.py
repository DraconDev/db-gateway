#!/usr/bin/env python3
"""Cross-database noisy-neighbor probe: does a read flood on one database
starve a second database?

Companion to scripts/soak_starvation.py, which deliberately contends a
SINGLE per-database guard (its `cold` database is never queried, so its
zeros cannot show cross-database anything). This probe puts real traffic on
`cold` while `hot` is flooded and reports cold's experience:

  phase A (baseline, --seconds): 2 writer threads on cold, hot idle.
  phase B (flood, --seconds):    10 reader threads scanning hot plus the
                                 same 2 cold writers.

The verdict compares cold's writer reject rate and latency between phases.
Exit codes: 0 COLD_OK, 1 COLD_STARVED (a real answer, like the sibling).

Usage: python3 scripts/noisy_neighbor_cross_db.py --out docs/<name>.json
       [--seconds 60] [--readers 10] [--dg-bin PATH]
Stdout is the human-readable evidence log; --out is one JSON document.
"""

import argparse
import atexit
import json
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

READINESS_TIMEOUT = 120
SCAN_ROWS = 4000
MAX_BATCH = 32


def call(base, key, method, path, body=None, timeout=30):
    headers = {
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/json",
    }
    req = urllib.request.Request(
        base + path,
        data=json.dumps(body or {}).encode(),
        method=method,
        headers=headers,
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
    except Exception:
        return (time.monotonic() - t0) * 1000.0, 599, {}


def loadavg1():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


def pct(samples, fraction):
    if not samples:
        return None
    ordered = sorted(samples)
    idx = min(len(ordered) - 1, int(fraction * len(ordered)))
    return round(ordered[idx], 2)


class Phase:
    """One measurement phase: N cold-writer threads, optionally hot readers."""

    def __init__(self, base, owner, seconds, readers):
        self.base = base
        self.owner = owner
        self.seconds = seconds
        self.readers = readers
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.ok = []
        self.rej = 0
        self.other = 0
        self.transport = 0
        self.attempts = 0

    def cold_writer(self, wid):
        n = 0
        while not self.stop.is_set():
            n += 1
            ms, st, _ = call(
                self.base, self.owner, "POST", "/v1/databases/cold/query",
                {"sql": "INSERT INTO kv(k, v) VALUES (?, ?)",
                 "args": [f"w{wid}-{n}-{time.monotonic_ns()}", "v"]})
            with self.lock:
                self.attempts += 1
                if st == 200:
                    self.ok.append(ms)
                elif st == 429:
                    self.rej += 1
                elif st == 599:
                    self.transport += 1
                else:
                    self.other += 1

    def hot_reader(self):
        while not self.stop.is_set():
            call(self.base, self.owner, "POST", "/v1/databases/hot/query",
                 {"sql": f"SELECT tag, score FROM events WHERE id <= {SCAN_ROWS}"})

    def db_counters(self, name):
        ms, st, body = call(self.base, self.owner, "GET", "/v1/status", {})
        if st != 200:
            return {}
        return (body.get("databases") or {}).get(name) or {}

    def run(self, label):
        cold_before = self.db_counters("cold")
        hot_before = self.db_counters("hot")
        load_before = loadavg1()
        writers = [threading.Thread(target=self.cold_writer, args=(i,))
                   for i in range(2)]
        readers = [threading.Thread(target=self.hot_reader)
                   for _ in range(self.readers)]
        for t in writers + readers:
            t.start()
        time.sleep(self.seconds)
        self.stop.set()
        for t in writers + readers:
            t.join()
        cold_after = self.db_counters("cold")
        hot_after = self.db_counters("hot")
        with self.lock:
            attempts, rej = self.attempts, self.rej
            other, transport = self.other, self.transport
            ok = list(self.ok)
        summary = {
            "label": label,
            "seconds": self.seconds,
            "cold_writers": 2,
            "hot_readers": self.readers,
            "cold_attempts": attempts,
            "cold_ok": len(ok),
            "cold_429": rej,
            "cold_other": other,
            "cold_transport": transport,
            "cold_reject_rate": round(rej / attempts, 5) if attempts else None,
            "cold_p50_ms": pct(ok, 0.50),
            "cold_p99_ms": pct(ok, 0.99),
            "cold_max_ms": round(max(ok), 2) if ok else None,
            "cold_counters_delta": {
                k: cold_after.get(k, 0) - cold_before.get(k, 0)
                for k in ("queue_waits", "queue_wait_ms", "queue_timeouts",
                          "rows_read", "rows_written")},
            "hot_counters_delta": {
                k: hot_after.get(k, 0) - hot_before.get(k, 0)
                for k in ("queue_waits", "queue_wait_ms", "queue_timeouts",
                          "rows_read", "rows_written")},
            "load1_before": load_before,
            "load1_after": loadavg1(),
        }
        print(f"# {label}: cold {len(ok)}/{attempts} ok, "
              f"{rej}x429 {other}other {transport}transport, "
              f"p50={summary['cold_p50_ms']} p99={summary['cold_p99_ms']} "
              f"load {load_before:.1f}->{summary['load1_after']:.1f}",
              flush=True)
        return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dg-bin", default=None,
                    help="path to the dg binary (default: .cargo/target/debug/dg)")
    ap.add_argument("--seconds", type=int, default=60,
                    help="duration of EACH phase (default 60)")
    ap.add_argument("--readers", type=int, default=10,
                    help="hot reader threads in the flood phase (default 10)")
    ap.add_argument("--out", required=True,
                    help="evidence JSON path (one document)")
    args = ap.parse_args()
    dg_bin = args.dg_bin or ".cargo/target/debug/dg"

    tmp = tempfile.mkdtemp(prefix="xdb-")
    atexit.register(shutil.rmtree, tmp, True)
    init = subprocess.run([dg_bin, "init", "--data-dir", tmp],
                          capture_output=True, text=True)
    assert init.returncode == 0, init.stderr
    owner = json.loads(init.stdout)["owner_key"]
    server = subprocess.Popen(
        [dg_bin, "serve", "--listen", "127.0.0.1:0", "--data-dir", tmp],
        stderr=subprocess.PIPE, text=True)
    lines = queue.Queue()

    def drain():
        for line in iter(server.stderr.readline, ""):
            lines.put(line)

    threading.Thread(target=drain, daemon=True).start()
    try:
        base = None
        deadline = time.monotonic() + READINESS_TIMEOUT
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("server never logged its listen address")
            if server.poll() is not None:
                raise AssertionError("server exited during readiness "
                                     f"rc={server.returncode}")
            try:
                line = lines.get(timeout=min(remaining, 0.25))
            except queue.Empty:
                continue
            try:
                addr = json.loads(line)["fields"].get("address")
            except Exception:
                addr = None
            if addr:
                base = f"http://{addr}"
                break
        assert base

        for name in ("hot", "cold"):
            ms, st, body = call(base, owner, "POST", "/v1/databases", {"name": name})
            assert st == 200, body
        ms, st, body = call(
            base, owner, "POST", "/v1/databases/hot/batch",
            {"statements": [{"sql": "CREATE TABLE events(id INTEGER PRIMARY KEY, "
                                    "tag TEXT, score REAL)"}], "atomic": True})
        assert st == 200, body
        ms, st, body = call(
            base, owner, "POST", "/v1/databases/cold/batch",
            {"statements": [{"sql": "CREATE TABLE kv(k TEXT PRIMARY KEY, "
                                    "v TEXT)"}], "atomic": True})
        assert st == 200, body
        seeded = 0
        while seeded < SCAN_ROWS:
            batch = min(MAX_BATCH, SCAN_ROWS - seeded)
            stmts = [{"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                      "args": [f"seed-{seeded + k}", float(k)]}
                     for k in range(batch)]
            ms, st, body = call(base, owner, "POST", "/v1/databases/hot/batch",
                                {"statements": stmts, "atomic": True})
            assert st == 200, body
            seeded += batch
        print(f"# seeded hot with {seeded} rows; load1={loadavg1():.1f}", flush=True)

        ms, st, body = call(base, owner, "GET", "/v1/status", {})
        admission = body.get("admission") if st == 200 else {}
        baseline = Phase(base, owner, args.seconds, 0).run("baseline")
        flood = Phase(base, owner, args.seconds, args.readers).run("flood")

        base_rate = baseline["cold_reject_rate"] or 0.0
        flood_rate = flood["cold_reject_rate"] or 0.0
        hard_fail = (baseline["cold_transport"] + baseline["cold_other"]
                     + flood["cold_transport"] + flood["cold_other"])
        if hard_fail:
            verdict = "COLD_STARVED"
            reason = (f"{hard_fail} cold writes failed with transport/other "
                      f"errors (not bounded 429s)")
        elif flood_rate <= max(0.01, 3 * base_rate):
            verdict = "COLD_OK"
            reason = (f"cold reject {base_rate:.3%} -> {flood_rate:.3%} under "
                      f"{args.readers}-reader hot flood; hot flood intensity "
                      f"{flood['hot_counters_delta'].get('rows_read')} rows read")
        else:
            verdict = "COLD_STARVED"
            reason = (f"cold reject {base_rate:.3%} -> {flood_rate:.3%} "
                      f"under hot flood")
        print(f"# verdict: {verdict}: {reason}", flush=True)
        report = {
            "schema": "dg-cross-db-probe/1",
            "admission": admission,
            "scan_rows": SCAN_ROWS,
            "hot_readers": args.readers,
            "verdict": {"verdict": verdict, "reason": reason},
            "baseline": baseline,
            "flood": flood,
        }
        with open(args.out, "w") as f:
            json.dump(report, f, indent=2)
            f.write("\n")
        return 0 if verdict == "COLD_OK" else 1
    finally:
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()


if __name__ == "__main__":
    sys.exit(main())
