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

TWO ARMS, ONE SERVER, BACK TO BACK -- this is the part that makes the
answer trustworthy on a shared machine:

  control  writers only. Establishes what a writer pays with NO read
           flood, on this machine, at this moment.
  flood    the same writers plus a read flood on the SAME hot database.

The verdict is driven by the DIFFERENCE between the arms, not by an absolute
latency threshold. External CPU contention (this rig is shared) inflates BOTH
arms roughly equally, so it cancels; only the read flood moves the writer
numbers. A differential verdict therefore stays valid when the machine is not
otherwise idle, while the absolute numbers are still recorded and are only
trustworthy when the sampled load is low. The script samples /proc/loadavg
into every status sample so the evidence log certifies its own contention.

Mechanism this is probing: `acquire_queue` polls `try_write_owned` every
2ms and never ENQUEUES in tokio's fair writer queue. A pending writer is
invisible to the lock, so readers released by the global read lane can
re-take the shared guard before the writer's next poll. The read lane has
READ_PERMITS=8, so >8 reader threads keep the guard continuously occupied.

Per-database counters come from GET /v1/status (src/api.rs), which exposes
`queue_waits`, `queue_wait_ms` and `queue_timeouts` per database. Both the
hot database (flooded) and an idle control database are reported, so
per-database attribution is visible rather than assumed.

Attribution invariant, checked in the summary: a writer that loses the
per-database queue gets Error::busy() (429 capacity_exceeded) AND bumps
that database's queue_timeouts AND bumps the global
`requests.capacity_exceeded`; a writer shed by the global write lane bumps
only `requests.capacity_exceeded`. With --writers <= WRITE_PERMITS no
writer is ever lane-shed, so writer 429s must equal the queue timeouts.
The summary prints both so a reviewer can see the identity held.

Usage:
  soak_starvation.py --seconds 300 --out /tmp/starvation.log
  soak_starvation.py --seconds 300 --flood-seconds 300 --out ...

Exit codes: 0 NO_STARVATION, 1 STARVED or MARGINAL (a real answer -- "yes,
build the writer-preferring bound"), 2 harness failure.
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
import urllib.error
import urllib.request
from pathlib import Path

READINESS_TIMEOUT = 120
# The range scan is tuned to hold a reader's shared guard for long enough for
# a writer to actually queue behind it. A bare point SELECT returns in
# microseconds and would never produce contention worth measuring. It is
# bounded by `id <= SCAN_ROWS`, so writers appending rows never widen the
# reader's unit of work -- the read guard hold time stays constant across
# both arms, which is what makes the arms comparable.
SCAN_ROWS = 4000
# execution.rs rejects a batch above 32 statements.
MAX_BATCH = 32
# Latency histogram resolution in ms. 0.05ms buckets keep sub-millisecond
# control-arm latencies distinguishable while bounding memory.
HIST_STEP = 0.05


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
        # A dropped connection is a starvation signal in its own right, so it
        # is reported as a transport status (599) rather than raised.
        return (time.monotonic() - t0) * 1000.0, 599, {}


def vmrss_kb(pid):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith("VmRSS:"):
                return int(line.split()[1])
    raise RuntimeError("no VmRSS line")


def loadavg1():
    with open("/proc/loadavg") as f:
        return float(f.read().split()[0])


class Latency:
    """Bounded millisecond histogram. Raw sample lists grow without limit
    (soak.py accumulated 1.28M requests worth), which both costs memory and
    makes the per-sample append lock a measurable part of the harness."""

    def __init__(self):
        self.lock = threading.Lock()
        self.buckets = {}
        self.count = 0
        self.errors = 0
        self.max_ms = 0.0

    def add(self, ms):
        idx = int(ms / HIST_STEP)
        with self.lock:
            self.buckets[idx] = self.buckets.get(idx, 0) + 1
            self.count += 1
            if ms > self.max_ms:
                self.max_ms = ms

    def percentile(self, fraction):
        with self.lock:
            if not self.count:
                return None
            # Nearest-rank on the cumulative histogram.
            target = fraction * self.count
            seen = 0
            for idx in sorted(self.buckets):
                seen += self.buckets[idx]
                if seen >= target:
                    return round(idx * HIST_STEP, 3)
        return None

    def snapshot(self):
        return {
            "count": self.count,
            "p50_ms": self.percentile(0.50),
            "p99_ms": self.percentile(0.99),
            "max_ms": round(self.max_ms, 2),
        }


class Stopper:
    def __init__(self):
        self._event = threading.Event()

    def set(self):
        self._event.set()

    def is_set(self):
        return self._event.is_set()


def status_of(base, key, hot, cold, timeout=10):
    """One /v1/status read, reduced to the fields the verdict needs."""
    lat, st, body = call(base, key, "GET", "/v1/status", {}, timeout=timeout)
    if st != 200:
        return None
    dbs = body.get("databases") or {}
    out = {"requests": body.get("requests") or {}, "admission": body.get("admission") or {}}
    for name in (hot, cold):
        entry = dbs.get(name) or {}
        out[name] = {
            "rows_read": entry.get("rows_read", 0),
            "rows_written": entry.get("rows_written", 0),
            "queue_waits": entry.get("queue_waits", 0),
            "queue_wait_ms": entry.get("queue_wait_ms", 0),
            "queue_timeouts": entry.get("queue_timeouts", 0),
        }
    return out


def delta_map(first, last, names):
    out = {}
    for name in names:
        a, b = first.get(name, {}), last.get(name, {})
        out[name] = {
            "queue_waits": b.get("queue_waits", 0) - a.get("queue_waits", 0),
            "queue_wait_ms": b.get("queue_wait_ms", 0) - a.get("queue_wait_ms", 0),
            "queue_timeouts": b.get("queue_timeouts", 0) - a.get("queue_timeouts", 0),
            "rows_read": b.get("rows_read", 0) - a.get("rows_read", 0),
            "rows_written": b.get("rows_written", 0) - a.get("rows_written", 0),
        }
    req = {}
    for k in ("total", "capacity_exceeded", "query_deadline_exceeded", "internal_error"):
        req[k] = (last["requests"].get(k, 0) - first["requests"].get(k, 0))
    out["_requests"] = req
    return out


def run_arm(base, key, hot, cold, args, label, seconds, readers, server_pid):
    """Run one arm and return its summary. Writers always run; readers only
    in the flood arm."""
    stop = Stopper()
    writer_lat, reader_lat = Latency(), Latency()
    counts = {"writer_429": 0, "writer_other": 0, "writer_transport": 0,
              "reader_429": 0, "reader_other": 0, "reader_transport": 0}
    counts_lock = threading.Lock()

    first = status_of(base, key, hot, cold)
    if first is None:
        raise AssertionError("could not read /v1/status before the arm")

    def bump(field):
        with counts_lock:
            counts[field] += 1

    def reader_thread(_worker):
        while not stop.is_set():
            lat, st, _ = call(
                base, key, "POST", f"/v1/databases/{hot}/query",
                {"sql": "SELECT count(*), avg(score) FROM events WHERE id <= ?",
                 "args": [SCAN_ROWS]},
            )
            reader_lat.add(lat)
            if st == 599:
                bump("reader_transport")
            elif st == 429:
                bump("reader_429")
            elif st != 200:
                bump("reader_other")

    def writer_thread(worker):
        seq = 0
        while not stop.is_set():
            seq += 1
            # Unique per arm+worker+seq, so no idempotent replay can mask a
            # lost ack and inflate the write count.
            key_id = f"starve-{label}-w{worker}-{seq}"
            lat, st, _ = call(
                base, key, "POST", f"/v1/databases/{hot}/query",
                {"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                 "args": [key_id, 1.0]},
                idem=key_id,
            )
            writer_lat.add(lat)
            if st == 599:
                bump("writer_transport")
            elif st == 429:
                bump("writer_429")
            elif st != 200:
                bump("writer_other")

    threads = [threading.Thread(target=writer_thread, args=(i,), daemon=True)
               for i in range(args.writers)]
    if readers:
        threads += [threading.Thread(target=reader_thread, args=(i,), daemon=True)
                    for i in range(readers)]
    for t in threads:
        t.start()

    samples = []
    rss_start = vmrss_kb(server_pid)
    arm_start = time.monotonic()
    next_sample = arm_start + 1.0
    while time.monotonic() - arm_start < seconds:
        now = time.monotonic()
        if now >= next_sample:
            snap = status_of(base, key, hot, cold)
            if snap is not None:
                samples.append({
                    "t": round(now - arm_start, 1),
                    "rss_kb": vmrss_kb(server_pid),
                    "load1": loadavg1(),
                    "queue_waits": snap[hot]["queue_waits"],
                    "queue_wait_ms": snap[hot]["queue_wait_ms"],
                    "queue_timeouts": snap[hot]["queue_timeouts"],
                    "capacity_exceeded": snap["requests"].get("capacity_exceeded", 0),
                })
            next_sample = now + 1.0
        time.sleep(0.05)
    stop.set()
    for t in threads:
        t.join(timeout=30)

    last = status_of(base, key, hot, cold)
    if last is None:
        raise AssertionError("could not read /v1/status after the arm")
    if not samples:
        raise AssertionError(f"{label}: never sampled /v1/status")
    elapsed = max(samples[-1]["t"] - samples[0]["t"], 1e-9)
    d = delta_map(first, last, (hot, cold))
    qtimeouts = d[hot]["queue_timeouts"]
    waits = d[hot]["queue_waits"]
    loads = [s["load1"] for s in samples]

    # Attribution: queue timeouts also land in the global capacity_exceeded
    # counter, so the lane-shed remainder is what is left over.
    lane_shed = d["_requests"]["capacity_exceeded"] - qtimeouts
    return {
        "arm": label,
        "seconds": round(elapsed, 1),
        "admission": first["admission"],
        "readers": readers,
        "writers": args.writers,
        "writer": writer_lat.snapshot(),
        "reader": reader_lat.snapshot(),
        "writer_429": counts["writer_429"],
        "writer_other": counts["writer_other"],
        "writer_transport": counts["writer_transport"],
        "reader_429": counts["reader_429"],
        "reader_other": counts["reader_other"],
        "reader_transport": counts["reader_transport"],
        "per_database": {hot: d[hot], cold: d[cold]},
        "queue_timeouts": qtimeouts,
        "queue_waits": waits,
        "waits_per_sec": round(waits / elapsed, 2),
        "mean_wait_ms": round(d[hot]["queue_wait_ms"] / waits, 2) if waits else 0.0,
        "requests": d["_requests"],
        "attribution": {
            "queue_timeouts": qtimeouts,
            "capacity_exceeded_total": d["_requests"]["capacity_exceeded"],
            "lane_shed_estimate": lane_shed,
            # Must be 0: no writer is ever shed by the global write lane, so
            # every writer 429 is a per-database queue timeout.
            "writer_429_equals_queue_timeouts":
                counts["writer_429"] == qtimeouts,
        },
        "load1_mean": round(sum(loads) / len(loads), 2),
        "load1_max": round(max(loads), 2),
        "rss_start_kb": rss_start,
        "rss_end_kb": samples[-1]["rss_kb"],
        "samples": samples,
    }


def decide(control, flood, args):
    """Differential verdict. Absolute p99 thresholds are only consulted as a
    secondary signal, because they are the part machine load can move."""
    c429, f429 = control["writer_429"], flood["writer_429"]
    cp99, fp99 = control["writer"]["p99_ms"], flood["writer"]["p99_ms"]
    ctimeouts = control["queue_timeouts"]
    ftimeouts = flood["queue_timeouts"]
    # Writer wait inflation caused by the read flood alone.
    ratio = None
    if fp99 is not None and cp99:
        ratio = round(fp99 / cp99, 1) if cp99 > 0 else None
    identity = (control["attribution"]["writer_429_equals_queue_timeouts"]
                and flood["attribution"]["writer_429_equals_queue_timeouts"])
    starved = f429 > 0 and c429 == 0
    marginal = (
        not starved
        and (f429 > c429
             or (ratio is not None and ratio >= 3.0
                 and fp99 is not None and fp99 >= args.writer_p99_ms))
    )
    if starved:
        verdict = "STARVED"
        why = (f"read flood pushed writer rejections from {c429} to {f429} "
               f"with no readers present in the control arm")
    elif marginal:
        verdict = "MARGINAL"
        why = (f"writer rejections {c429}->{f429}, writer p99 {cp99}->{fp99}ms "
               f"(x{ratio}) -- degraded but not rejected")
    else:
        verdict = "NO_STARVATION"
        why = (f"writer rejections stayed at {f429} under the flood; "
               f"writer p99 {cp99}->{fp99}ms (x{ratio})")
    return {
        "verdict": verdict,
        "reason": why,
        "writer_p99_inflation_x": ratio,
        "control_writer_429": c429,
        "flood_writer_429": f429,
        "control_queue_timeouts": ctimeouts,
        "flood_queue_timeouts": ftimeouts,
        "absolute_p99_threshold_ms": args.writer_p99_ms,
        "attribution_identity_held": identity,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dg-bin", default=None,
                    help="path to the dg binary (default: .cargo/target/debug/dg)")
    ap.add_argument("--seconds", type=int, default=300,
                    help="duration of EACH arm (default 300)")
    ap.add_argument("--flood-seconds", type=int, default=None,
                    help="duration of the flood arm only (default: --seconds)")
    ap.add_argument("--control-seconds", type=int, default=None,
                    help="duration of the control arm only (default: --seconds)")
    ap.add_argument("--readers", type=int, default=10,
                    help="concurrent reader threads in the flood arm; must "
                         "exceed READ_PERMITS=8 to hold the shared guard "
                         "continuously (default 10, just over the lane)")
    ap.add_argument("--writers", type=int, default=4,
                    help="concurrent writer threads in BOTH arms; must not "
                         "exceed WRITE_PERMITS=4 or the global write lane "
                         "sheds writers and pollutes the measurement")
    ap.add_argument("--writer-p99-ms", type=float, default=100.0,
                    help="secondary absolute p99 signal; a starved writer is "
                         "rejected at ACQUIRE_TIMEOUT=500ms, so anything near "
                         "this is already broken (default 100)")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    if args.dg_bin is None:
        args.dg_bin = str(Path(__file__).resolve().parent.parent
                          / ".cargo" / "target" / "debug" / "dg")
    if args.writers < 1 or args.readers < 1:
        ap.error("--writers and --readers must both be >= 1")
    if args.writers > 4:
        ap.error("--writers must be <= WRITE_PERMITS (4) or the global write "
                 "lane sheds writers and the queue measurement is void")
    if args.seconds < 30:
        ap.error("--seconds must be >= 30 to mean anything")
    flood_seconds = args.flood_seconds or args.seconds
    control_seconds = args.control_seconds or args.seconds
    if flood_seconds < 30 or control_seconds < 30:
        ap.error("each arm must be >= 30 seconds")

    readiness_timeout = float(os.environ.get(
        "SOAK_READINESS_TIMEOUT", str(READINESS_TIMEOUT)))
    if readiness_timeout <= 0:
        ap.error("readiness timeout must be positive")

    tmp = tempfile.mkdtemp(prefix="starve-")
    init = subprocess.run([args.dg_bin, "init", "--data-dir", tmp],
                          capture_output=True, text=True)
    assert init.returncode == 0, init.stderr
    owner = json.loads(init.stdout)["owner_key"]

    server = subprocess.Popen(
        [args.dg_bin, "serve", "--listen", "127.0.0.1:0", "--data-dir", tmp],
        stderr=subprocess.PIPE, text=True)
    stderr_lines = queue.Queue()

    def drain_stderr():
        for line in iter(server.stderr.readline, ""):
            stderr_lines.put(line)

    threading.Thread(target=drain_stderr, daemon=True).start()

    try:
        base = None
        deadline = time.monotonic() + readiness_timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError("server never logged its listen address")
            if server.poll() is not None:
                raise AssertionError(f"server exited during readiness "
                                     f"rc={server.returncode}")
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

        # ONE hot database is the whole point: contention must happen on a
        # single per-database guard. `cold` exists only so the per-database
        # counters in the report are visibly per-database and not global.
        HOT, COLD = "hot", "cold"
        for name in (HOT, COLD):
            lat, st, body = call(base, owner, "POST", "/v1/databases", {"name": name})
            assert st == 200, body
        lat, st, body = call(
            base, owner, "POST", f"/v1/databases/{HOT}/batch",
            {"statements": [{"sql": "CREATE TABLE events(id INTEGER PRIMARY KEY, "
                                    "tag TEXT, score REAL)"}],
             "atomic": True})
        assert st == 200, body

        seeded = 0
        while seeded < SCAN_ROWS:
            batch = min(MAX_BATCH, SCAN_ROWS - seeded)
            stmts = [{"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                      "args": [f"seed-{seeded + k}", float(k)]}
                     for k in range(batch)]
            lat, st, body = call(base, owner, "POST",
                                 f"/v1/databases/{HOT}/batch",
                                 {"statements": stmts, "atomic": True})
            assert st == 200, body
            seeded += batch

        control = run_arm(base, owner, HOT, COLD, args, "control",
                          control_seconds, 0, server.pid)
        time.sleep(3)  # settle between arms so the baseline is clean
        flood = run_arm(base, owner, HOT, COLD, args, "flood",
                        flood_seconds, args.readers, server.pid)

        assert server.poll() is None, f"server died mid-run rc={server.returncode}"
        if flood["writer"]["count"] < 10:
            raise AssertionError(
                f"only {flood['writer']['count']} writes completed; run too short")

        verdict = decide(control, flood, args)
        summary = {
            "verdict": verdict,
            "control": {k: v for k, v in control.items() if k != "samples"},
            "flood": {k: v for k, v in flood.items() if k != "samples"},
            "admission": flood["admission"],
            "scan_rows": SCAN_ROWS,
        }
        with open(args.out, "a") as log:
            log.write(json.dumps(summary) + "\n")
            for arm in (control, flood):
                log.write("# samples " + json.dumps(
                    {"arm": arm["arm"], "samples": arm["samples"]}) + "\n")
        print(json.dumps(summary), flush=True)
        return 1 if verdict["verdict"] in ("STARVED", "MARGINAL") else 0
    finally:
        if server.poll() is None:
            server.kill()
        server.wait()


if __name__ == "__main__":
    sys.exit(main())
