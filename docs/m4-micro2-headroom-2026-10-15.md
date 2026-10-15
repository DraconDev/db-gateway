# M4 headroom evidence — micro2 steady-state + restore (2026-10-15)

All numbers below were sampled live from micro2 (145.241.238.20) over
SSH between 23:22 and 23:31 UTC, read-only except for a restore into a
disposable `/var/tmp` staging dir (removed after verification). No
production data was touched: the only database read was `verify_probe`
(row count), and the restore target was a temp copy.

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

## Headroom vs docs/resource-budget.md

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
half of it.

## Drift note (action for the next deploy change)

The on-host gateway cap is **64M** (`systemctl show dg.service
-MemoryMax`), while `deploy/dg.service` in-tree says `MemoryMax=96M`.
The 64M value still clears every measurement above by ~6x, so nothing
is at risk, but the template and the host disagree and the budget doc
quotes 96M. The next change that touches the unit should set one value
in both places and re-record this file's table. This file reports the
host fact; it does not resolve the drift.
