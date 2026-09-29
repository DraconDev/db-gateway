#!/usr/bin/env python3
"""Derive the numbers a rehearsal packet actually contains, as markdown.

Run: python3 scripts/packet-facts.py docs/sandbox-packet-2026-09-29-rehearsal.log
     python3 scripts/packet-facts.py /path/to/full-packet.md --json

WHY THIS EXISTS. Three times, the docs quoted a measured number that the
committed packet did not contain -- gate-9 peaks from a run nobody had kept, a
corruption-case count from a later script than the one that produced the
artifact, an operation matrix that did not sum to its own stated total. Each
time the prose and the evidence had been written at different moments, and
nothing checked them against each other. This prints the facts from a packet so
the two can be written from ONE source, and so a reviewer can recompute them
instead of trusting them.

The request counts come from the folded per-request JSONL. A committed packet
elides that log for size (`--out-of-band` is not needed: the counts are also in
the summary lines), so a packet without the JSONL reports the totals it can
still read and says which ones it could not.

Attribution: litestream stamps every upload with `x-amz-meta-litestream-timestamp`
and signs with the Go AWS SDK, which leaves `amz-sdk-*` markers in the signed
header set. `s3probe.py` is a pure-Python signer and produces neither, so the two
clients are separable in the log -- which matters because the litestream half is
what a least-privilege credential policy is written from.
"""

from __future__ import annotations

import argparse
import collections
import json
import re
import sys

# (method, query-prefix) -> how the two clients' requests look in the log
KEYS = [
    "list-type=2",
    "versions",
    "lifecycle",
    "x-id=PutObject",
    "x-id=GetObject",
    "versionId",
]


def load_requests(text: str) -> list[dict]:
    out: list[dict] = []
    for block in re.findall(r"```json\n(.*?)\n```", text, re.S):
        for line in block.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                out.append(json.loads(line))
            except ValueError:
                continue
    seen: set[str] = set()
    uniq: list[dict] = []
    for r in out:
        k = json.dumps(r, sort_keys=True)
        if k in seen:
            continue
        seen.add(k)
        uniq.append(r)
    return uniq


def who(r: dict) -> str:
    if r.get("litestream_meta"):
        return "litestream"
    if "amz-sdk-invocation-id" in (r.get("signed_headers") or ""):
        return "litestream"
    return "packet"


def classify(r: dict) -> str:
    q = r.get("query") or ""
    for k in KEYS:
        if k in q:
            return k
    if "versionId" in q:
        return "versionId"
    return "/<bucket>" if not (r.get("key") or "") else "(object key)"


def matrix(reqs: list[dict]) -> tuple[list[tuple[str, str, int, str]], int]:
    c: collections.Counter = collections.Counter()
    for r in reqs:
        c[(r.get("method", "?"), classify(r), who(r))] += 1
    rows = sorted(c.items(), key=lambda kv: (kv[0][0], -kv[1]))
    return [(m, p, n, w) for (m, p, w), n in rows], len(reqs)


def scalar_facts(text: str) -> dict[str, str]:
    facts: dict[str, str] = {}

    def grab(pattern: str, name: str, cast=str) -> None:
        m = re.search(pattern, text)
        if m:
            facts[name] = cast(m.group(1))

    grab(r"gates recorded: (\d+)\s+pass: (\d+)", "gates_recorded", str)
    m = re.search(r"gates recorded: (\d+)\s+pass: (\d+)\s+blocked: (\d+)\s+fail: (\d+)", text)
    if m:
        facts["gates_recorded"] = m.group(1)
        facts["pass"] = m.group(2)
        facts["blocked"] = m.group(3)
        facts["fail"] = m.group(4)
    m = re.search(r"gates executed: (\d+)/(\d+) distinct \((\d+) gate sections", text)
    if m:
        facts["gates_executed"] = f"{m.group(1)}/{m.group(2)}"
        facts["gate_sections"] = m.group(3)
    grab(r"folded (\d+) endpoint requests", "requests", str)
    grab(r"steady replication\s+peak RSS (\d+) KiB", "replication_peak_kib", str)
    grab(r"restore\s+(\d+) ms, (\d+) rows restored, peak child RSS (\d+) KiB", "restore_ms", str)
    m = re.search(r"restore\s+(\d+) ms, (\d+) rows restored, peak child RSS (\d+) KiB", text)
    if m:
        facts["restore_ms"] = m.group(1)
        facts["restore_rows"] = m.group(2)
        facts["restore_peak_kib"] = m.group(3)
    grab(r"injected 403s seen verbatim in the daemon log, whole drill: (\d+)", "injected_403s", str)
    grab(r'"versions_enumerated": (\d+)', "versions_enumerated", str)
    grab(r'"as_is": \{"kept": (\d+)', "versions_kept", str)
    grab(r'"if_everything_were_31_days_older": \{"kept": (\d+)', "versions_kept_if_aged", str)
    grab(r"of 4 were REJECTED, (\d+) matched the source digest", "damage_matched", str)
    grab(r"corrupt artifacts \((\d+) downloaded", "damage_downloaded", str)
    m = re.search(r"canary reads it back with no key|verdict\": \"(\w[\w-]*)\"", text)
    if m:
        facts["sse_canary"] = m.group(1)
    m = re.search(r'"verdict": "([\w-]+)"', text)
    if m:
        facts["sse_canary"] = m.group(1)
    facts["gate10_status"] = (
        "BLOCKED" if "## gate 10: BLOCKED" in text else
        "PASS" if "## gate 10: PASS" in text else
        "FAIL" if "## gate 10: FAIL" in text else "absent"
    )
    facts["gate6_status"] = (
        "BLOCKED" if "## gate 6: BLOCKED" in text else
        "PASS" if "## gate 6: PASS" in text else
        "FAIL" if "## gate 6: FAIL" in text else "absent"
    )
    return facts


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("packet", help="a packet file, with or without the raw request log")
    ap.add_argument("--json", action="store_true", help="emit JSON instead of markdown")
    ap.add_argument("--write-matrix", metavar="FILE",
                    help="write the derived matrix + totals as a compact JSON sidecar")
    ap.add_argument("--from-matrix", metavar="FILE",
                    help="take the matrix from a sidecar instead of the packet's request log; "
                         "a committed packet elides that log for size, and a sidecar is what "
                         "keeps the operation matrix checkable from the repository")
    args = ap.parse_args(argv)

    try:
        text = open(args.packet, encoding="utf-8").read()
    except OSError as exc:
        print(f"packet-facts: {exc}", file=sys.stderr)
        return 2

    reqs = load_requests(text)
    facts = scalar_facts(text)
    rows: list[tuple[str, str, int, str]] = []
    if args.from_matrix:
        try:
            side = json.load(open(args.from_matrix, encoding="utf-8"))
        except OSError as exc:
            print(f"packet-facts: {exc}", file=sys.stderr)
            return 2
        facts["matrix_source"] = args.from_matrix
        facts["requests_in_log"] = str(side.get("requests", "unknown"))
        for r in side.get("matrix", []):
            rows.append((r["method"], r["operation"], int(r["count"]), r["issued_by"]))
        if args.write_matrix:
            print(f"packet-facts: matrix came from {args.from_matrix}; nothing new to write",
                  file=sys.stderr)
            args.write_matrix = None
    elif reqs:
        rows, total = matrix(reqs)
        facts["requests_in_log"] = str(total)
        non2xx = collections.Counter(r.get("status") for r in reqs if int(r.get("status") or 0) >= 300)
        facts["non_2xx"] = str(sum(non2xx.values()))
        facts["non_2xx_by_status"] = json.dumps(
            {str(k): v for k, v in sorted(non2xx.items(), key=lambda kv: int(kv[0]))}
        )
        facts["sse_c_signed_requests"] = str(sum(
            1 for r in reqs
            if "x-amz-server-side-encryption-customer-algorithm" in (r.get("signed_headers") or "")
        ))
    else:
        facts["requests_in_log"] = "not available: the per-request log is elided from this copy"

    if args.write_matrix:
        payload = {
            "derived_from": args.packet,
            "requests": facts.get("requests_in_log"),
            "non_2xx": facts.get("non_2xx"),
            "non_2xx_by_status": facts.get("non_2xx_by_status"),
            "sse_c_signed_requests": facts.get("sse_c_signed_requests"),
            "matrix": [{"method": m, "operation": p, "count": n, "issued_by": w}
                       for m, p, n, w in rows],
        }
        with open(args.write_matrix, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=1, sort_keys=True)
            fh.write("\n")
        print(f"packet-facts: wrote {args.write_matrix} ({sum(r[2] for r in rows)} requests)",
              file=sys.stderr)

    if args.json:
        print(json.dumps({"facts": facts,
                          "matrix": [{"method": m, "operation": p, "count": n, "issued_by": w}
                                     for m, p, n, w in rows]},
                         indent=2, sort_keys=True))
        return 0

    print("## facts this packet contains\n")
    if "matrix_source" in facts:
        print(f"(matrix taken from the sidecar {facts['matrix_source']}; the packet's own "
              f"request log is elided from the committed copy)")
    for k, v in facts.items():
        print(f"- {k}: {v}")
    if rows:
        print("\n## operation matrix\n")
        print("| method | query-prefix | count | issued by |")
        print("| --- | --- | --- | --- |")
        for m, p, n, w in rows:
            print(f"| `{m}` | `{p}` | {n} | {w} |")
        print(f"\nrows sum to {sum(r[2] for r in rows)}; the log holds {len(reqs)} records")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
