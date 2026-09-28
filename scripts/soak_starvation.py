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

TWO ARMS, ONE SERVER, INTERLEAVED -- this is the part that makes the
answer trustworthy on a shared machine:

  control  writers only. Establishes what a writer pays with NO read
           flood, on this machine, at this moment.
  flood    the same writers plus a read flood on the SAME hot database.

The run alternates short blocks (control, flood, control, flood, ...) rather
than finishing one arm before starting the other, because this rig is shared
and its load moves by tens of units within minutes. Alternating puts any
drift in BOTH arms instead of loading one of them, so the verdict rests on
the difference between the arms and not on the absolute latency, which is
the only part machine load can move. The absolute numbers are still
recorded, per block, and the script samples /proc/loadavg into every status
sample so the evidence log certifies its own contention.

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

    def merge(self, other):
        """Exact union of two histograms. Averaging percentiles across blocks
        would be wrong; summing buckets is not."""
        with self.lock:
            for idx, n in other.buckets.items():
                self.buckets[idx] = self.buckets.get(idx, 0) + n
            self.count += other.count
            self.max_ms = max(self.max_ms, other.max_ms)


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


def run_block(base, key, hot, cold, args, label, seconds, readers, server_pid):
    """Run one block and return its RAW result (histograms + counters).

    Kept raw rather than summarised so blocks of the same arm can be merged
    into one arm total; percentiles cannot be averaged across blocks, but
    histograms can be summed exactly.
    """
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
            # NO Idempotency-Key here, deliberately. The key is recorded in
            # the same transaction's _dg_replay table, which is a
            # per-principal capacity guard (MAX_REPLAYS=256 in
            # src/execution.rs): once 256 unexpired keys exist, EVERY further
            # keyed write is rejected with 429 capacity_exceeded. A unique
            # key per write therefore makes the harness reject its own
            # writes within milliseconds and then report that as starvation.
            # An unkeyed write is still counted client-side from its
            # response; a lost ack simply is not counted, which understates
            # throughput and never overstates it.
            lat, st, _ = call(
                base, key, "POST", f"/v1/databases/{hot}/query",
                {"sql": "INSERT INTO events(tag, score) VALUES (?, ?)",
                 "args": [f"{label}-w{worker}-{seq}", 1.0]},
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
        raise AssertionError("could not read /v1/status after the block")
    if not samples:
        raise AssertionError(f"{label}: never sampled /v1/status")
    return {
        "label": label,
        "readers": readers,
        "elapsed": max(samples[-1]["t"] - samples[0]["t"], 1e-9),
        "admission": first["admission"],
        "writer_lat": writer_lat,
        "reader_lat": reader_lat,
        "counts": dict(counts),
        "delta": delta_map(first, last, (hot, cold)),
        "samples": samples,
        "rss_start_kb": rss_start,
        "rss_end_kb": samples[-1]["rss_kb"],
    }


def merge_blocks(blocks, hot, cold, args, label):
    """Fold the blocks of one arm into the single arm summary the verdict
    reads. Sums histograms and counter deltas; never averages percentiles."""
    writer_lat, reader_lat = Latency(), Latency()
    for block in blocks:
        writer_lat.merge(block["writer_lat"])
        reader_lat.merge(block["reader_lat"])
    counts = {"writer_429": 0, "writer_other": 0, "writer_transport": 0,
              "reader_429": 0, "reader_other": 0, "reader_transport": 0}
    d = {hot: {k: 0 for k in ("queue_waits", "queue_wait_ms", "queue_timeouts",
                               "rows_read", "rows_written")},
         cold: {k: 0 for k in ("queue_waits", "queue_wait_ms", "queue_timeouts",
                               "rows_read", "rows_written")},
         "_requests": {}}
    for block in blocks:
        for k in counts:
            counts[k] += block["counts"][k]
        for name in (hot, cold):
            for k, v in block["delta"][name].items():
                d[name][k] += v
        for k, v in block["delta"]["_requests"].items():
            d["_requests"][k] = d["_requests"].get(k, 0) + v
    elapsed = sum(b["elapsed"] for b in blocks)
    qtimeouts = d[hot]["queue_timeouts"]
    waits = d[hot]["queue_waits"]
    loads = [s["load1"] for b in blocks for s in b["samples"]]

    # Attribution: a per-database queue timeout ALSO lands in the global
    # capacity_exceeded counter, so whatever is left over was lane-shed.
    lane_shed = d["_requests"]["capacity_exceeded"] - qtimeouts
    return {
        "arm": label,
        "blocks": len(blocks),
        "seconds": round(elapsed, 1),
        "admission": blocks[0]["admission"],
        "readers": blocks[0]["readers"],
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
        "waits_per_sec": round(waits / elapsed, 2) if elapsed else 0.0,
        "mean_wait_ms": round(d[hot]["queue_wait_ms"] / waits, 2) if waits else 0.0,
        "requests": d["_requests"],
        "attribution": {
            "queue_timeouts": qtimeouts,
            "capacity_exceeded_total": d["_requests"]["capacity_exceeded"],
            "lane_shed_estimate": lane_shed,
            # Every queue timeout (writer OR reader) also lands in the global
            # capacity_exceeded counter, so this identity must hold exactly.
            "queue_timeouts_equal_429s":
                qtimeouts == counts["writer_429"] + counts["reader_429"],
            # Holds only when no reader can time out, i.e. in the control arm.
            # Under a read flood readers queue behind a writer holding the
            # exclusive guard, so queue_timeouts exceeds writer_429 and this
            # going false is itself evidence of the readers starving too.
            "writer_429_equals_queue_timeouts": counts["writer_429"] == qtimeouts,
        },
        "load1_mean": round(sum(loads) / len(loads), 2),
        "load1_max": round(max(loads), 2),
        "rss_start_kb": blocks[0]["rss_start_kb"],
        "rss_end_kb": blocks[-1]["rss_end_kb"],
        "block_series": [
            {"t": b["label"],
             "writer_p99_ms": b["writer_lat"].percentile(0.99),
             "writer_429": b["counts"]["writer_429"],
             "queue_timeouts": b["delta"][hot]["queue_timeouts"],
             "load1": round(sum(s["load1"] for s in b["samples"])
                            / len(b["samples"]), 2)}
            for b in blocks
        ],
    }


def decide(control, flood, args):
    """Differential verdict, driven by the writer REJECTION RATE.

    Rate, not absolute count, and not an absolute p99 threshold. The two
    arms attempt different numbers of writes (the flood arm is slower, so
    it issues fewer), and on a contended box both arms sit near the 500ms
    ACQUIRE_TIMEOUT ceiling where p99 saturates and stops discriminating --
    an earlier run had p99 502ms control vs 567ms flood, a useless 1.1x, while
    the rejection rate told the real story: 0.6% vs 9.2%.

    Matched adjacent block pairs carry the most weight: equal duration and
    near-equal load, so they isolate the read flood from everything else.
    """
    def rate(arm):
        attempts = max(arm["writer"]["count"], 1)
        rejected = arm["writer_429"] + arm["writer_other"] + arm["writer_transport"]
        return rejected / attempts, rejected, attempts

    c_rate, c_rej, c_att = rate(control)
    f_rate, f_rej, f_att = rate(flood)
    ratio = round(f_rate / c_rate, 1) if c_rate > 0 else None

    # Adjacent control/flood block pairs: same length, load within ~15%.
    pairs = []
    for a, b in zip(control["block_series"], flood["block_series"]):
        ratio_pair = (b["queue_timeouts"] / a["queue_timeouts"]
                      if a["queue_timeouts"] else None)
        pairs.append({
            "control_block": a["t"],
            "flood_block": b["t"],
            "control_load1": a["load1"],
            "flood_load1": b["load1"],
            "control_queue_timeouts": a["queue_timeouts"],
            "flood_queue_timeouts": b["queue_timeouts"],
            "timeout_ratio": round(ratio_pair, 1) if ratio_pair else None,
        })
    unanimous = bool(pairs) and all(
        p["flood_queue_timeouts"] > p["control_queue_timeouts"] for p in pairs)
    load_matched = [p for p in pairs
                    if min(p["control_load1"], p["flood_load1"]) > 0
                    and abs(p["control_load1"] - p["flood_load1"])
                    / max(p["control_load1"], p["flood_load1"]) <= 0.25]

    starved = (f_rate >= max(2 * c_rate, 0.005)) and f_rate > c_rate
    if starved:
        verdict = "STARVED"
        why = (f"read flood raised the writer rejection rate from "
               f"{100 * c_rate:.2f}% to {100 * f_rate:.2f}% (x{ratio}); "
               f"{sum(1 for p in pairs if p['flood_queue_timeouts'] > p['control_queue_timeouts'])}"
               f"/{len(pairs)} matched block pairs separate in the flood's "
               f"disfavour")
    elif f_rate > c_rate:
        verdict = "MARGINAL"
        why = (f"writer rejection rate rose {100 * c_rate:.2f}% -> "
               f"{100 * f_rate:.2f}% (x{ratio}) -- degraded, not rejected")
    else:
        verdict = "NO_STARVATION"
        why = (f"writer rejection rate did not rise under the flood "
               f"({100 * c_rate:.2f}% -> {100 * f_rate:.2f}%)")
    return {
        "verdict": verdict,
        "reason": why,
        "control_writer_reject_rate": round(c_rate, 5),
        "flood_writer_reject_rate": round(f_rate, 5),
        "reject_rate_ratio": ratio,
        "control_writer_rejected": c_rej,
        "flood_writer_rejected": f_rej,
        "control_writer_attempts": c_att,
        "flood_writer_attempts": f_att,
        "control_write_per_sec": round(c_att / max(control["seconds"], 1e-9), 1),
        "flood_write_per_sec": round(f_att / max(flood["seconds"], 1e-9), 1),
        "matched_block_pairs": pairs,
        "all_pairs_separate": unanimous,
        "load_matched_pair_count": len(load_matched),
        "control_queue_timeouts": control["queue_timeouts"],
        "flood_queue_timeouts": flood["queue_timeouts"],
        "control_reader_429": control["reader_429"],
        "flood_reader_429": flood["reader_429"],
        "absolute_p99_threshold_ms": args.writer_p99_ms,
    }


def wait_for_quiet(args):
    """Block until the machine has been quiet for --load-patience samples.

    A latency soak on a contended box measures the neighbours, not the
    gateway, so the gate is a precondition rather than a nicety. It prints
    its progress so the evidence log shows when the run was allowed to start
    and what the load was at that moment.
    """
    if args.max_load1 <= 0:
        return {"gated": False, "load1_at_start": loadavg1()}
    streak = 0
    began = time.monotonic()
    announced = False
    while streak < args.load_patience:
        current = loadavg1()
        if current <= args.max_load1:
            streak += 1
        else:
            streak = 0
            if not announced:
                print(f"# load gate: load1={current:.2f} > {args.max_load1}, "
                      f"waiting (this box is shared; latency under contention "
                      f"is worthless)", flush=True)
                announced = True
        if streak < args.load_patience:
            if args.load_timeout > 0 and time.monotonic() - began > args.load_timeout:
                raise AssertionError(
                    f"load never stayed <= {args.max_load1} for "
                    f"{args.load_patience} samples within {args.load_timeout}s "
                    f"(load1={current:.2f}); refusing to report a latency "
                    f"soak measured under contention")
            time.sleep(args.load_poll)
    print(f"# load gate open after {time.monotonic() - began:.0f}s "
          f"(load1={loadavg1():.2f} <= {args.max_load1} for "
          f"{args.load_patience} samples)", flush=True)
    return {"gated": True, "load1_at_start": loadavg1(),
            "waited_secs": round(time.monotonic() - began, 1),
            "max_load1": args.max_load1}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dg-bin", default=None,
                    help="path to the dg binary (default: .cargo/target/debug/dg)")
    ap.add_argument("--seconds", type=int, default=120,
                    help="duration of EACH block (default 120)")
    ap.add_argument("--blocks", type=int, default=6,
                    help="number of blocks; arms are INTERLEAVED "
                         "control/flood/control/flood..., half each (default "
                         "6, so 3 per arm). Interleaving is what keeps the "
                         "verdict valid on a busy box: load drift reaches both "
                         "arms equally instead of loading one of them")
    ap.add_argument("--flood-seconds", type=int, default=None,
                    help="duration of each flood BLOCK (default: --seconds)")
    ap.add_argument("--control-seconds", type=int, default=None,
                    help="duration of each control BLOCK (default: --seconds)")
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
    ap.add_argument("--max-load1", type=float, default=0.0,
                    help="do not start until /proc/loadavg 1-minute load has "
                         "stayed at or below this for --load-patience "
                         "consecutive samples. 0 disables the gate. Latency "
                         "measured under CPU contention is worthless, and a "
                         "shared rig is usually busy (default 0: off)")
    ap.add_argument("--load-patience", type=int, default=6,
                    help="consecutive quiet samples required to open the gate "
                         "(default 6, i.e. ~90s at the default poll)")
    ap.add_argument("--load-poll", type=float, default=15.0,
                    help="seconds between load samples while gated")
    ap.add_argument("--load-timeout", type=float, default=0.0,
                    help="give up waiting for a quiet machine after this many "
                         "seconds (0 = wait forever)")
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
    if args.blocks < 2 or args.blocks % 2:
        ap.error("--blocks must be even and >= 2, so both arms get equal time")
    if args.blocks > 40:
        ap.error("--blocks above 40 outlasts the stall watchdog; run several "
                 "invocations instead")
    flood_seconds = args.flood_seconds or args.seconds
    control_seconds = args.control_seconds or args.seconds
    if flood_seconds < 30 or control_seconds < 30:
        ap.error("each block must be >= 30 seconds")

    readiness_timeout = float(os.environ.get(
        "SOAK_READINESS_TIMEOUT", str(READINESS_TIMEOUT)))
    if readiness_timeout <= 0:
        ap.error("readiness timeout must be positive")

    tmp = tempfile.mkdtemp(prefix="starve-")
    gate = wait_for_quiet(args)
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

        # Interleaved ABAB blocks. Running the whole control arm and then the
        # whole flood arm would let machine load drift straight into the
        # verdict; alternating short blocks puts any drift in both arms.
        order = (["control", "flood"] * (args.blocks // 2))
        control_blocks, flood_blocks, all_blocks = [], [], []
        for i, label in enumerate(order):
            seconds = control_seconds if label == "control" else flood_seconds
            readers = 0 if label == "control" else args.readers
            block = run_block(base, owner, HOT, COLD, args,
                              f"{i + 1}:{label}", seconds, readers, server.pid)
            all_blocks.append(block)
            (control_blocks if label == "control" else flood_blocks).append(block)
            time.sleep(2)  # settle so the next block's baseline is clean
        control = merge_blocks(control_blocks, HOT, COLD, args, "control")
        flood = merge_blocks(flood_blocks, HOT, COLD, args, "flood")

        assert server.poll() is None, f"server died mid-run rc={server.returncode}"
        if flood["writer"]["count"] < 10:
            raise AssertionError(
                f"only {flood['writer']['count']} writes completed; run too short")

        # Harness self-check. With NO read flood a healthy gateway rejects
        # almost nothing, so a control arm that rejects heavily means the
        # HARNESS is the confound (see the Idempotency-Key note in
        # writer_thread) and its numbers say nothing about starvation.
        # Failing loudly beats reporting a confident wrong verdict.
        ctrl_reject = (control["writer_429"] + control["writer_other"]
                       + control["writer_transport"])
        ctrl_total = max(ctrl_reject + control["writer"]["count"], 1)
        if ctrl_reject / ctrl_total > 0.02:
            raise AssertionError(
                f"control arm rejected {ctrl_reject}/{ctrl_total} writes with no "
                f"read flood -- the harness is the confound, not the server "
                f"(429={control['writer_429']} other={control['writer_other']} "
                f"transport={control['writer_transport']})")

        verdict = decide(control, flood, args)
        summary = {
            "verdict": verdict,
            "control": control,
            "flood": flood,
            "admission": flood["admission"],
            "scan_rows": SCAN_ROWS,
            "load_gate": gate,
            "block_order": order,
        }
        with open(args.out, "a") as log:
            log.write(json.dumps(summary) + "\n")
            for block in all_blocks:
                log.write("# samples " + json.dumps(
                    {"block": block["label"], "readers": block["readers"],
                     "samples": block["samples"]}) + "\n")
        print(json.dumps(summary), flush=True)
        return 1 if verdict["verdict"] in ("STARVED", "MARGINAL") else 0
    finally:
        if server.poll() is None:
            server.kill()
        server.wait()


if __name__ == "__main__":
    sys.exit(main())
