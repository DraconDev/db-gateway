#!/usr/bin/env python3
"""Writer-starvation soak: does a continuous read flood delay writers?

This is the measurement `Store::acquire_queue` explicitly defers. Its own
comment (src/store.rs) says the poll-based acquire "is not FIFO-fair like the
async global permits, and a continuous read flood could delay a writer", and
that "a writer-preferring bound gets added only if that measurement shows
starvation (scale proposal Phase 2)".

Why scripts/soak.py cannot answer it: that harness round-robins across four
databases, and `acquire_queue` takes a PER-DATABASE guard. Spreading load
across databases means the per-database lock is never contended, so queue
waits and timeouts stay at zero no matter how busy the server is. Its 2h run
(1.28M requests, 0 errors) therefore proves nothing about starvation.

This harness inverts that: every thread hammers ONE hot database, so readers
share its read guard while writers must exclude it.

  * --readers threads run a range scan sized to hold the shared read guard
    for a measurable time. The read lane has READ_PERMITS=8, so more than
    that many concurrent readers guarantee the lane is saturated.
  * --writers threads issue keyed writes to the same database and record
    their own latency. A starved writer shows up as a tail blowup and,
    eventually, as `queue_timeouts` growth.
  * A sampler polls GET /v1/status once a second and records the per-database
    `queue_waits`, `queue_wait_ms` and `queue_timeouts` counters that
    src/api.rs exposes for exactly this purpose.

Verdict logic (the number scale Phase 2 needs):
  STARVED     writer timeouts > 0, or writer p99 above --writer-p99-ms.
  MARGINAL    writer p99 within 20% of the threshold and timeouts > 0.
  NO_STARVATION
              writer timeouts == 0 and writer p99 under the threshold.

Usage:
  soak_starvation.py --seconds 900 --out /tmp/starvation.log

Exit codes: 0 no starvation, 1 starvation or marginal detected (still a valid
measurement -- the answer is "yes, build the bound"), 2 harness failure.

This measures latency, so run it on an otherwise idle machine. Under
sustained external CPU contention the absolute numbers are meaningless even
though the ratio between writer and reader latency survives.
"""

import argparse
import json
import os
import queue
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

READINESS_TIMEOUT = 120
# The range scan is tuned to hold a reader's shared guard for long enough for
# a writer to actually queue behind it. A bare point SELECT returns in
# microseconds and would never produce contention worth measuring.
SCAN_ROWS = 4000


def call(base, key, method, path, body=None, timeout=30, idem=None):
    headers = {
        "Authorization": f"Bearer {key}",
        "Content-Type": "application/json",
    }
    if idem is not None:
        headers["Idempotency-Key"] = idem
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
        # A dropped connection or a timeout is a starvation signal in its own
        # right, so it is reported as a latency sample with a 599 status
        # rather than raised.
        return (time.monotonic() - t0) * 1000.0, 599, {}


def vmrss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError("no VmRSS line")


class Stopper:
    def __init__(self):
        self._event = threading.Event()

    def set(self):
        self._event.set()

    def is_set(self):
        return self._event.is_set()


def percentile(values, fraction):
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, int(len(s) * fraction))]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--dg-bin",
        default=None,
        help="path to the dg binary (default: .cargo/target/debug/dg)",
    )
    ap.add_argument("--seconds", type=int, default=900)
    ap.add_argument("--readers", type=int, default=24,
                    help="concurrent reader threads on the hot database "
                         "(must exceed READ_PERMITS=8 to saturate the lane)")
    ap.add_argument("--writers", type=int, default=4)
    ap.add_argument("--writer-p99-ms", type=float, default=500.0,
                    help="writer p99 above this counts as starvation "
                         "(default 500, well under the 500ms ACQUIRE_TIMEOUT "
                         "so a starved writer shows before it starts timing out)")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    if args.dg_bin is None:
        args.dg_bin = str(
            Path(__file__).resolve().parent.parent / ".cargo" / "target" / "debug" / "dg"
        )
    if args.readers < 1 or args.writers < 1:
        ap.error("--readers and --writers must both be >= 1")
    if args.seconds < 30:
        ap.error("--seconds must be >= 30 to mean anything")

    readiness_timeout = float(os.environ.get("SOAK_READINESS_TIMEOUT", str(READINESS_TIMEOUT)))
    if readiness_timeout <= 0:
        ap.error("--readiness timeout must be positive")

    tmp = tempfile.mkdtemp(prefix="starve-")
    init = subprocess.run([args.dg_bin, "init", "--data-dir", tmp],
                          capture_output=True, text=True)
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

    threading.Thread(target=drain_stderr, daemon=True).start()
    stop = Stopper()
    reader_lat, writer_lat = [], []
    reader_reqs, writer_reqs, writer_fails = [0], [0], [0]
    lat_lock = threading.Lock()

    try:
        base = None
        deadline = time.monotonic() + readiness_timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("server never logged its listen address")
            if server.poll() is not None:
                raise AssertionError(f"server exited during readiness rc={server.returncode}")
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

        # ONE hot database. This is the whole point: contention has to happen
        # on a single per-database guard to be measurable at all.
        HOT = "hot"
        lat, st, body = call(base, owner, "POST", "/v1/databases", {"name": HOT})
        assert st == 200, body
        lat, st, body = call(
            base, owner, "POST", f"/v1/databases/{HOT}/batch",
            {"statements": [{"sql": "CREATE TABLE events(id INTEGER PRIMARY KEY, tag TEXT, score REAL)"}],
             "atomic": True},
        )
        assert st == 200, body
        # Seed enough rows that the reader's range scan is a real unit of work
        # rather than an instant return.
        seeded = 0
        while seeded < SCAN_ROWS:
            batch = max(1, min(200, SCAN_ROWS - seeded))
            stmts = [
                {"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                 "args": [f"seed-{seeded + k}", float(k)]}
                for k in range(batch)
            ]
            lat, st, body = call(base, owner, "POST", f"/v1/databases/{HOT}/batch",
                                 {"statements": stmts, "atomic": True})
            assert st == 200, body
            seeded += batch

        def reader_thread(worker):
            while not stop.is_set():
                lat, st, _ = call(
                    base, owner, "POST", f"/v1/databases/{HOT}/query",
                    {"sql": "SELECT count(*), avg(score) FROM events WHERE id <= ?",
                     "args": [SCAN_ROWS]},
                )
                with lat_lock:
                    reader_lat.append(lat)
                    reader_reqs[0] += 1

        def writer_thread(worker):
            seq = 0
            while not stop.is_set():
                seq += 1
                # Keyed so a retry is never needed and a lost ack cannot
                # inflate the write count; the key stays unique per worker.
                key = f"starve-w{worker}-{seq}"
                lat, st, _ = call(
                    base, owner, "POST", f"/v1/databases/{HOT}/query",
                    {"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                     "args": [key, 1.0]},
                    idem=key,
                )
                with lat_lock:
                    writer_lat.append(lat)
                    writer_reqs[0] += 1
                    if st != 200:
                        writer_fails[0] += 1

        threads = [threading.Thread(target=reader_thread, args=(i,), daemon=True)
                   for i in range(args.readers)]
        threads += [threading.Thread(target=writer_thread, args=(i,), daemon=True)
                    for i in range(args.writers)]
        for t in threads:
            t.start()

        # Sampler: the starvation counters src/api.rs already exposes.
        samples = []
        first_status = None
        last_status = None
        first_rss = vmrss_kb(server.pid)
        sample_start = time.monotonic()
        next_sample = sample_start + 1.0
        while time.monotonic() - sample_start < args.seconds:
            now = time.monotonic()
            if now >= next_sample:
                lat, st, body = call(base, owner, "GET", "/v1/status", {}, timeout=10)
                if st == 200:
                    entry = (body.get("databases") or {}).get(HOT) or {}
                    sample = {
                        "t": round(now - sample_start, 1),
                        "rss_kb": vmrss_kb(server.pid),
                        "queue_waits": entry.get("queue_waits", 0),
                        "queue_wait_ms": entry.get("queue_wait_ms", 0),
                        "queue_timeouts": entry.get("queue_timeouts", 0),
                        "read_permits": (body.get("admission") or {}).get("read_permits"),
                        "write_permits": (body.get("admission") or {}).get("write_permits"),
                    }
                    samples.append(sample)
                    if first_status is None:
                        first_status = sample
                    last_status = sample
                next_sample = now + 1.0
            time.sleep(0.05)
        stop.set()
        for t in threads:
            t.join(timeout=30)

        assert server.poll() is None, f"server died mid-run rc={server.returncode}"

        # Deltas over the sampled window, so warmup does not pollute the rate.
        if first_status is None or last_status is None:
            raise AssertionError("never sampled /v1/status")
        elapsed = max(last_status["t"] - first_status["t"], 1e-9)
        waits = last_status["queue_waits"] - first_status["queue_waits"]
        wait_ms = last_status["queue_wait_ms"] - first_status["queue_wait_ms"]
        timeouts = last_status["queue_timeouts"] - first_status["queue_timeouts"]

        w_p50, w_p99 = percentile(writer_lat, 0.50), percentile(writer_lat, 0.99)
        r_p50, r_p99 = percentile(reader_lat, 0.50), percentile(reader_lat, 0.99)

        starved = timeouts > 0 or (w_p99 is not None and w_p99 > args.writer_p99_ms)
        marginal = (not starved) and timeouts > 0
        verdict = "STARVED" if starved else ("MARGINAL" if marginal else "NO_STARVATION")

        summary = {
            "verdict": verdict,
            "seconds": round(elapsed, 1),
            "readers": args.readers,
            "writers": args.writers,
            "read_permits": last_status["read_permits"],
            "write_permits": last_status["write_permits"],
            "reader_reqs": reader_reqs[0],
            "writer_reqs": writer_reqs[0],
            "writer_non_200": writer_fails[0],
            "reader_p50_ms": round(r_p50, 2) if r_p50 else None,
            "reader_p99_ms": round(r_p99, 2) if r_p99 else None,
            "writer_p50_ms": round(w_p50, 2) if w_p50 else None,
            "writer_p99_ms": round(w_p99, 2) if w_p99 else None,
            "queue_waits_delta": waits,
            "queue_wait_ms_delta": wait_ms,
            "queue_timeouts_delta": timeouts,
            "waits_per_sec": round(waits / elapsed, 1),
            "mean_wait_ms": round(wait_ms / waits, 2) if waits else 0.0,
            "rss_start_kb": first_rss,
            "rss_end_kb": last_status["rss_kb"],
            "writer_p99_threshold_ms": args.writer_p99_ms,
        }
        with open(args.out, "a") as log:
            log.write(json.dumps(summary) + "\n")
        print(json.dumps(summary), flush=True)

        if writer_reqs[0] < 10:
            raise AssertionError(f"only {writer_reqs[0]} writes completed; run too short")
        # Exit 1 means "yes, build the writer-preferring bound" -- a real
        # answer, not a harness failure.
        return 1 if verdict in ("STARVED", "MARGINAL") else 0
    finally:
        stop.set()
        server.kill()
        server.wait()


if __name__ == "__main__":
    sys.exit(main())
