# M4 headroom evidence — micro2 steady-state + restore (2026-10-15/16)

Superseding note (2026-10-16): the table below was first recorded from
ad-hoc SSH sampling. It has since been re-collected by the checked-in
script `scripts/m4-micro2-sampling.sh`, whose verbatim transcript is
`docs/evidence/m4-micro2-sampling-2026-10-16.log` (3 passes, gateway 6696 KiB
steady, sidecar ~21300 KiB, restore 446 ms, integrity ok, 5=5). Treat
the transcript as the inspectable record; this file remains the human
summary. Read-only throughout except a restore into disposable `/var/tmp`
staging (removed after verification).

## Host context

- `Linux micro2 6.17.0-1019-oracle #19~24.04.3-Ubuntu SMP x86_64`
- 2 vCPU, MemTotal 954 MiB, MemAvailable 422–454 MiB across the window,
  load 0.00–0.07, `/` 45G with 26G available (42% used).
- `dg.service`: active, `MainPID=439701`, `User=dg`,
  **`MemoryMax=67108864` (64M)** — see the drift note below.
- `dg-litestream.service`: active, `MainPID=479600`, `MemoryMax=100663296`
  (96M). `dg-backup.timer`: active, next elapse 00:17 UTC.
- Gateway self-report (`/v1/status`, version 0.1.0, uptime 98660 s):
  `total 187, capacity_exceeded 0, internal_error 0,
  query_deadline_exceeded 0`, per-database `queue_timeouts 0`.

## Steady-state CPU/RSS (6 samples, ~50 s apart)

| t (UTC) | gw RSS KiB | gw CPU ticks | sidecar RSS KiB | sidecar CPU ticks | MemAvailable KiB |
|---|---|---|---|---|---|
| 23:22:14 | 6620 | 6055 | 20904 | 5165 | 429584 |
| 23:23:04 | 6620 | 6057 | 20684 | 5167 | 428564 |
| 23:23:55 | 6620 | 6059 | 20752 | 5170 | 454336 |
| 23:24:45 | 6620 | 6062 | 20756 | 5173 | 450816 |
| 23:25:35 | 6620 | 6064 | 20756 | 5176 | 443504 |
| 23:26:26 | 6620 | 6066 | 21112 | 5178 | 441076 |

Plus: gateway `VmHWM 9260 KiB, VmSize 287452 KiB, Threads 5`;
sidecar `VmHWM 36240 KiB, Threads 8`. After a handful of live SELECTs
the gateway sat at 6696 KiB — no growth signature.
CPU deltas over ~4.2 min on 2 CPUs: gateway +11 ticks, sidecar +13
ticks — both effectively idle, as expected for a host serving only
hourly snapshots plus probes.

`litestream status`: `verify_probe.sqlite ok txid 0000000000000001 wal
4.2 kB`. Latest snapshot set `20261015T231717Z`, `dg backup verify`
`ok:true has_registry:true`, `/v1/backups` reports `verify_probe`
`fresh` (age 666 s at query time).

## Restore (litestream, to disposable staging)

- `litestream restore -config /etc/litestream.yml -o
  /var/tmp/lit-restore-m4-measure/verify_probe.sqlite
  /var/lib/dg/verify_probe.sqlite` (with `/etc/dg/litestream.env`
  sourced): **exit 0, 759 ms, 24576 bytes**.
- Sidecar RSS during/after: `VmRSS 20740 KiB, VmHWM 36240 KiB` — the
  restore did not move the HWM.
- Pulled copy: `PRAGMA integrity_check → ok`, `SELECT count(*) FROM t
  → 5`, matching the live API count of 5. User tables in the copy: `t`
  (plus `_dg_replay`, `_litestream_seq`, `_litestream_lock` internals).
- Staging dir removed after verification.

Two honest failure modes were hit on the way there and are recorded so
the next operator does not rediscover them: without the env file the
restore fails reaching S3 (`no EC2 IMDS role found`, exit 1), and with
the env sourced but run from `/` it fails creating its temp path
(`permission denied`, exit 1). The restore needs the service env plus a
writable CWD — an operator note, not a defect.

## Headroom vs docs/resource-budget.md — conditional, not a fit claim

This section records steady-idle inputs only. Target-host burst at the
429 capacity edge (budget item 4) and disk-growth projection (item 5)
are not measured here, so the budget position stays "no claim of fit"
(`docs/resource-budget.md`, conditional conclusion). What is recorded:

| Term | This run | Budget reference |
|---|---|---|
| Gateway steady RSS (micro2) | 6620 KiB | M4 sample 8292 KiB; release idle 8480 (M2r) / 8236 (today, 1G scope) |
| Gateway burst peak (local, 1G-enforced scope, today) | 11192 KiB, settle 8976 | M2r 13056; 2026-10-11 1G-scope 11844; code ceiling 24 MiB (M3) |
| Sidecar steady RSS (micro2) | ~20740 KiB | Rehearsal replication peak 45632 / restore 34576 (gate 9) |
| Sidecar HWM (micro2) | 36240 KiB | Same rehearsal row |
| Host free | ~430 MiB of 954 | OS + page cache is the remainder term |

Composed worst case under the enforced caps: 64M (gateway unit) + 96M
(sidecar unit) = 160 MiB against a 954 MiB host showing ~430 MiB free
at idle — roughly 5x headroom over the caps, ~35x over measured steady
RSS (~27 MiB combined). The SQLite cache ceiling (24 MiB, M3) remains
the binding gateway term; the measured burst peak (11 MiB) sits under
half of it. None of this certifies the host under burst or growth — it
puts the idle fact next to the local-burst fact so the remaining
measurements have a baseline to beat.

## Failure-visibility scope (local proxies + one live revoke)

The revoke/egress/disk triple in `docs/evidence/m4-failure-visibility-2026-10-15.json`
(exit 0, re-runnable by `scripts/m4-failure-visibility-drill.sh`) runs
against a disposable local gateway by safety necessity: blocking egress
means firewall changes and filling disk means host-wide pressure, and
neither is an acceptable unattended action on the live host. The one
failure that was exercised on micro2 itself is key revocation — the
e2e campaign created, used, and revoked a scoped key against the live
gateway (`docs/evidence/e2e-test-campaign-2026-10-15.log` §4.3: read-only
role enforced via `sql_forbidden`, revoked key denied, owner writes
unaffected). Egress and disk visibility therefore rest on local
proxies honestly labeled as such in the drill script.

## Drift note (resolved 2026-10-16)

The on-host gateway cap was **64M** while `deploy/dg.service` in-tree
said `MemoryMax=96M`. Resolved toward the host fact: the template now
sets `MemoryMax=64M` with the on-target measurement as justification
(~2.7x cache ceiling, ~6x measured burst peak), and the budget's
owner-decision note records the revision. The sidecar keeps its own
96M cap in its own unit.
