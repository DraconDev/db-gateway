#!/usr/bin/env python3
"""Cross-database noisy-neighbor probe: does a flood on one database starve
a second database?

Companion to scripts/soak_starvation.py, which deliberately contends a
SINGLE per-database guard (its `cold` database is never queried, so its
zeros cannot show cross-database anything). This probe puts real traffic on
`cold` while `hot` is flooded on BOTH global lanes and reports cold's
experience:

  baseline1 (--seconds): 2 writer threads on cold, hot idle.
  flood     (--seconds): 4 writer threads + 10 reader threads on hot,
                         plus the same 2 cold writers.
  baseline2 (--seconds): 2 writer threads on cold, hot idle again.

Hot readers use `SELECT COUNT(*) ... WHERE id <= SCAN_ROWS`: one returned
row (far under the 1000-row result wall) that still holds hot's shared
guard ~45ms per scan, like the sibling's flood. A full-row scan would be
rejected by the result wall before touching storage and would only prove
the admission layer exists. Hot writers saturate the 4-permit write lane
the cold writers also draw from; hot readers saturate the 8-permit read
lane. Flanking baselines (ABA) separate flood effect from box-load drift:
if the baselines themselves shed, the box is too hot and the verdict says
so instead of certifying.

Exit codes: 0 COLD_OK, 1 COLD_STARVED, 2 BOX_TOO_HOT (rerun when quiet).

Usage: python3 scripts/noisy_neighbor_cross_db.py --out docs/<name>.json
       [--seconds 60] [--readers 10] [--hot-writers 4] [--dg-bin PATH]
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


class Counts:
    def __init__(self):
        self.lock = threading.Lock()
        self.attempts = 0
        self.ok = []
        self.rej = 0
        self.other = 0
        self.transport = 0

    def add(self, ms, st):
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

    def summary(self):
        with self.lock:
            ok = list(self.ok)
            return {
                "attempts": self.attempts,
                "ok": len(ok),
                "429": self.rej,
                "other": self.other,
                "transport": self.transport,
                "reject_rate": round(self.rej / self.attempts, 5) if self.attempts else None,
                "p50_ms": pct(ok, 0.50),
                "p99_ms": pct(ok, 0.99),
                "max_ms": round(max(ok), 2) if ok else None,
            }


class Phase:
    def __init__(self, base, owner, seconds, hot_readers, hot_writers):
        self.base = base
        self.owner = owner
        self.seconds = seconds
        self.hot_readers = hot_readers
        self.hot_writers = hot_writers
        self.stop = threading.Event()
        self.cold = Counts()
        self.hot_w = Counts()
        self.hot_r = Counts()

    def cold_writer(self, wid):
        n = 0
        while not self.stop.is_set():
            n += 1
            ms, st, _ = call(
                self.base, self.owner, "POST", "/v1/databases/cold/query",
                {"sql": "INSERT INTO kv(k, v) VALUES (?, ?)",
                 "args": [f"w{wid}-{n}-{time.monotonic_ns()}", "v"]})
            self.cold.add(ms, st)

    def hot_writer(self, wid):
        n = 0
        while not self.stop.is_set():
            n += 1
            ms, st, _ = call(
                self.base, self.owner, "POST", "/v1/databases/hot/query",
                {"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                 "args": [f"f{wid}-{n}-{time.monotonic_ns()}", 1.5]})
            self.hot_w.add(ms, st)

    def hot_reader(self):
        while not self.stop.is_set():
            ms, st, _ = call(
                self.base, self.owner, "POST", "/v1/databases/hot/query",
                {"sql": f"SELECT COUNT(*) FROM events WHERE id <= {SCAN_ROWS}"})
            self.hot_r.add(ms, st)

    def db_counters(self, name):
        ms, st, body = call(self.base, self.owner, "GET", "/v1/status", {})
        if st != 200:
            return {}
        return (body.get("databases") or {}).get(name) or {}

    def run(self, label):
        cold_before = self.db_counters("cold")
        hot_before = self.db_counters("hot")
        load_before = loadavg1()
        threads = ([threading.Thread(target=self.cold_writer, args=(i,)) for i in range(2)]
                   + [threading.Thread(target=self.hot_writer, args=(i,))
                      for i in range(self.hot_writers)]
                   + [threading.Thread(target=self.hot_reader)
                      for _ in range(self.hot_readers)])
        for t in threads:
            t.start()
        time.sleep(self.seconds)
        self.stop.set()
        for t in threads:
            t.join()
        cold_after = self.db_counters("cold")
        hot_after = self.db_counters("hot")

        def delta(before, after):
            return {k: after.get(k, 0) - before.get(k, 0)
                    for k in ("queue_waits", "queue_wait_ms", "queue_timeouts",
                              "rows_read", "rows_written")}

        summary = {
            "label": label,
            "seconds": self.seconds,
            "cold": self.cold.summary(),
            "hot_writers": self.hot_w.summary() if self.hot_writers else None,
            "hot_readers": self.hot_r.summary() if self.hot_readers else None,
            "cold_counters_delta": delta(cold_before, cold_after),
            "hot_counters_delta": delta(hot_before, hot_after),
            "load1_before": load_before,
            "load1_after": loadavg1(),
        }
        c = summary["cold"]
        print(f"# {label}: cold {c['ok']}/{c['attempts']} ok, "
              f"{c['429']}x429 {c['other']}other {c['transport']}transport, "
              f"p50={c['p50_ms']} p99={c['p99_ms']} "
              f"load {load_before:.1f}->{summary['load1_after']:.1f}", flush=True)
        if self.hot_writers or self.hot_readers:
            print(f"# {label} flood intensity: hot writers "
                  f"{summary['hot_writers']['ok']}/{summary['hot_writers']['attempts']} ok "
                  f"({summary['hot_writers']['429']}x429), hot readers "
                  f"{summary['hot_readers']['ok']}/{summary['hot_readers']['attempts']} ok "
                  f"({summary['hot_readers']['429']}x429)", flush=True)
        return summary


def pooled(base1, base2):
    attempts = base1["cold"]["attempts"] + base2["cold"]["attempts"]
    rej = base1["cold"]["429"] + base2["cold"]["429"]
    hard = (base1["cold"]["transport"] + base1["cold"]["other"]
            + base2["cold"]["transport"] + base2["cold"]["other"])
    return attempts, rej, hard


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dg-bin", default=None,
                    help="path to the dg binary (default: .cargo/target/debug/dg)")
    ap.add_argument("--seconds", type=int, default=60,
                    help="duration of EACH phase (default 60)")
    ap.add_argument("--readers", type=int, default=10,
                    help="hot reader threads in the flood phase (default 10)")
    ap.add_argument("--hot-writers", type=int, default=4,
                    help="hot writer threads in the flood phase (default 4)")
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
        b1 = Phase(base, owner, args.seconds, 0, 0).run("baseline1")
        fl = Phase(base, owner, args.seconds, args.readers, args.hot_writers).run("flood")
        b2 = Phase(base, owner, args.seconds, 0, 0).run("baseline2")

        attempts, rej, hard = pooled(b1, b2)
        base_rate = rej / attempts if attempts else 0.0
        f, fhard = fl["cold"]["429"], fl["cold"]["transport"] + fl["cold"]["other"]
        flood_rate = f / fl["cold"]["attempts"] if fl["cold"]["attempts"] else 0.0
        hw = fl["hot_writers"]
        hr = fl["hot_readers"]
        flood_real = (hw["ok"] > 100 and hr["ok"] > 100)
        if not flood_real:
            verdict, reason, code = (
                "BOX_TOO_HOT",
                f"hot flood never materialized (writers {hw['ok']}/{hw['attempts']}, "
                f"readers {hr['ok']}/{hr['attempts']}); measurement invalid",
                2)
        elif base_rate > 0.02 or hard:
            verdict, reason, code = (
                "BOX_TOO_HOT",
                f"baselines themselves shed ({rej}/{attempts} rejects, {hard} hard "
                f"errors): box too contended to measure; rerun when quiet",
                2)
        elif fhard:
            verdict, reason, code = (
                "COLD_STARVED",
                f"{fhard} cold flood writes failed with transport/other errors "
                f"(not bounded 429s)",
                1)
        elif flood_rate <= max(0.01, 3 * base_rate):
            verdict, reason, code = (
                "COLD_OK",
                f"cold reject {base_rate:.3%} -> {flood_rate:.3%} under hot flood "
                f"(writers {hw['ok']}/{hw['attempts']}, readers {hr['ok']}/{hr['attempts']})",
                0)
        else:
            verdict, reason, code = (
                "COLD_STARVED",
                f"cold reject {base_rate:.3%} -> {flood_rate:.3%} under hot flood",
                1)
        print(f"# verdict: {verdict}: {reason}", flush=True)
        report = {
            "schema": "dg-cross-db-probe/1",
            "admission": admission,
            "scan_rows": SCAN_ROWS,
            "hot_readers": args.readers,
            "hot_writers": args.hot_writers,
            "verdict": {"verdict": verdict, "reason": reason},
            "baseline1": b1,
            "flood": fl,
            "baseline2": b2,
        }
        with open(args.out, "w") as f:
            json.dump(report, f, indent=2)
            f.write("\n")
        return code
    finally:
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()


if __name__ == "__main__":
    sys.exit(main())
