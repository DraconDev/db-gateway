# Recovery policy — canonical tiers, objectives, retention

**Document role:** The one canonical statement of which backup mechanism
is authoritative for which failure, what objectives it is held to, and
what is retained where. It supersedes the tier language in
`recovery-decisions.md` (research brief), `personal-turso-megaplan.md`
("first tier" phrasing), and `observability-contract.md` wherever they
disagree; those documents keep their evidence and rationale. Artifact
shape and restore verification remain `recovery-contract.md`'s job;
this document selects policy, it does not redefine artifacts.

Status: **working policy for the supervised micro2 deployment, not an
accepted production SLA.** Profile C objectives below are the owner's
2026-09-27 working target (`recovery-decisions.md` §4/§6.1), adopted
"not as a guarantee". Production acceptance (signing the RPO/RTO as
production commitments) is still a separate owner decision and nothing
here claims it.

## The two tiers, by failure class

There is exactly one authoritative tier per failure class. The two
mechanisms are complements, not competitors; the confusion this
document ends was both being called "the first tier" in different
documents while each silently lacked something the other had.

| | **Tier 1 — host loss** | **Tier 2 — local rollback** |
|---|---|---|
| Mechanism | Litestream 0.5.17 continuous WAL replication to OVH object storage (`dg-litestream.service`), one replica prefix per database plus `metadata` for the registry | Hourly verified snapshot sets on the same disk (`dg-backup.timer` → `micro2-backup.sh` → `/var/lib/dg-backups`) |
| Authoritative for | Host/disk loss, host compromise, any failure where micro2's disk is gone or untrusted | Operator error, bad writes, botched lifecycle ops, quick same-host restore |
| Contains | Every live database **and the registry** (`metadata.sqlite`: credential hashes, scopes, database rows) | Every live database plus `metadata.sqlite.bak`, manifest + checksums (`recovery-contract.md` §2) |
| RPO objective | **5 minutes** after committed changes (Profile C target, successful verified operation only; an outage extends it and must be visible, never silent) | 1 hour (the producer cadence) |
| RTO objective | **15 minutes** to a verified readable copy on a separate target once a replacement host/binary exists (Profile C target; host provisioning itself is outside the objective) | Minutes (same-host file copy + `dg backup verify`) |
| Retention | Frequent points **7 days** in-replica (`retention: 168h`, `snapshot-interval: 24h`, rendered by `scripts/litestream-sync.sh`); daily-version classes **30 production / 7 development** are bucket-lifecycle policy — see "not yet enforced" below | Newest **24 sets** (`--keep 24` ≈ 24 h at the hourly cadence) |
| Proof on cadence | Weekly `dg-restore-drill.timer` runs `scripts/bucket-restore-drill.sh`: restore every database + registry from replicas to a separate target, verify integrity + per-table row counts against the live gateway | Every set is verified before publication by the producer; re-verifiable any time with `dg backup verify`; daily digest reports per-database freshness (`dg-backup-digest.timer`) |
| Point-in-time | Every moment inside the retained WAL window can be restored to, not only the last snapshot: `scripts/restore-point.sh --apply --to <ISO8601> --database <name>` restores into a fresh directory, verifies the copy and publishes what it produced. The window itself is published by `--list` and reported in `GET /v1/backups` (`restore_points`) | — (the sets are whole copies; there is nothing between them) |

Tier 2 is **explicitly not host-loss protection**: the sets live on the
same disk as the databases. Its value is speed and a registry copy
that travels with a consistent manifest for same-host accidents.

### The approved snapshot cadence

The Tier 2 RPO objective — **1 hour, the producer cadence** — is also the
owner-approved *snapshot cadence* the backup-health alarm is judged against
(observability contract §2a). It is configured on the host as
`--backup-cadence-secs 3600` on `dg serve` and `dg backup digest`.

The two numbers are the same for one reason: the producer publishes hourly,
so a change is legitimately unrepresented for up to an hour. The cadence is
what tells that wait apart from a producer that has stopped. Set it below
the producer's interval and the digest reports every change as overdue;
set it far above it and a stopped producer goes unnoticed. The objective is
a target, not a promise — an outage extends it and must stay visible
(§2), which is why the cadence is the floor for reporting, never a reason
to stop looking.

## Registry and credentials from the bucket alone (Tier 1 recovery)

The registry replicates continuously as the fixed `metadata` entry in
the sync-rendered Litestream config, so the bucket alone can rebuild a
working gateway — closing the gap where Tier 1 previously restored
data files whose scoped keys no longer existed anywhere. Procedure:

1. Provision the replacement host, install the `dg` release bundle and
   Litestream (`docs/deployment.md`), do **not** run `dg init` (that
   would mint a new owner over the restored registry).
2. Restore `metadata.sqlite` and every `<db>.sqlite` from their
   replica prefixes into the empty data directory
   (`litestream restore -config …`), mode/ownership as the live host.
3. Start `dg serve` on loopback; verify with the **existing** owner key
   file: `dg list` shows every database, `dg backup verify`/row counts
   spot-check against expectations, and one scoped key per application
   authenticates and reads.
4. **Skew check (mandatory):** registry and data files replicate
   independently, so a restored pair can straddle an admin action from
   seconds before the loss. A scoped key minted inside that window may
   be absent from the restored registry, and a database created inside
   it may have a replica but no registry row (or the reverse). Detect
   by the step-3 checks (`dg list` vs replica prefixes; one auth probe
   per application). Repair is re-issuing the affected key or
   re-importing the affected database file — bounded, manual, and
   recorded, never by editing the restored registry.
5. If the restored registry fails integrity or scope checks entirely,
   fall back to rebuild: `dg init` a new owner, `dg import` each
   restored data file, re-issue all scoped keys. Data survives;
   credentials deliberately do not (hash-only storage makes the
   originals unrecoverable by design).

Proven locally under a **live** gateway — a scoped key minted after
replication started reached the registry replica, and after total
data-dir loss both pre-loss keys authenticated with scope intact on a
gateway rebuilt from the replica alone:
`scripts/registry-recovery-drill.sh`,
`docs/evidence/registry-recovery-drill-2026-10-19.log`. Sandbox gate 11 proved
the same file round-trip against a live OVH bucket with the gateway
stopped (`docs/evidence/sandbox-packet-2026-10-14-ovh.log`).

## What is not yet enforced (residuals, named not hidden)

- **Production bucket lifecycle unverified.** The 30/7 daily-version
  classes are enforced by bucket lifecycle + versioning, not by
  Litestream (it never deletes). Gate 6 proved OVH stores and returns
  the candidate 7/30-day policy on a sandbox bucket; whether the
  production bucket has that lifecycle applied has **not** been read
  back (no S3 read tooling on the host, and the production secret does
  not leave it). Until an owner-approved read-back (or application)
  happens, daily-version retention beyond the in-replica 168 h is a
  target, not a fact. In-replica retention (168 h + daily snapshots)
  is rendered by the sync script and takes effect when the updated
  script is deployed.
- **Encryption choice still open** (`recovery-decisions.md` item 3):
  SSE-C is sent by the client and was refused-without-key on OVH in
  gate 10, but the deployed config does not set `sse-customer-key`;
  current at-rest state is the bucket's SSE-OMK default. Choosing
  SSE-C is an owner decision with key-custody consequences; this
  policy records the gap rather than picking silently.
- **Profile C is a target.** The 5-minute RPO has rehearsal and
  live-sidecar evidence behind it, but lag under sustained write load
  on the 1 GB host is not continuously measured; the weekly restore
  drill plus `litestream status`/coverage checks in host-audit are
  the standing signals, and a missed objective is a finding to
  investigate (per the 2026-09-27 decision), not a breached SLA.
- **Registry in the live config** landed 2026-10-19 (see the live
  enablement record below). Host-audit's coverage check treats a
  missing registry entry as failure from that change onward.

## Decision record

- Profile C objectives: owner, 2026-09-27 (working target, not a
  guarantee) — `recovery-decisions.md` §4.
- Two-tiers-by-failure-class and registry-in-Tier-1: audit A5,
  2026-10-19 (`docs/audit-2026-10-19.md`), judgment recorded in the
  goal ledger; alternatives (hourly registry snapshot file,
  rebuild-only) rejected with reasons there and in
  `docs/evidence/registry-recovery-drill-2026-10-19.log`.
- Restore-always-to-separate-target and 7-day rollback-artifact
  retention: `recovery-decisions.md` item 4 (unchanged).

## Live enablement record (2026-10-19, owner-approved)

- Updated `litestream-sync.sh` deployed via `scripts/install-litestream.sh`
  (idempotent; only-copy verified). The render added `metadata` to the
  live config; `dg-litestream.service` was restarted once onto it and
  `litestream status` shows `metadata.sqlite ok` (txid 2, 948 kB WAL —
  live registry replication under the serving gateway). The explicit
  `retention: 168h` / `snapshot-interval: 24h` lines entered the live
  config with the same render.
- `scripts/bucket-restore-drill.sh` installed as
  `/usr/local/bin/bucket-restore-drill.sh`; `dg-restore-drill.timer`
  enabled (first scheduled fire Sun 2026-10-25 05:30 UTC). First
  manual run: all databases + registry restored from the real bucket
  replicas and verified, exit 0
  (`db=billing_dev_pilot state=ok`, `db=verify_probe state=ok`,
  `db=metadata state=ok creds=2 missing=[]`).
- Host-audit after enablement: verdict pass, `litestream_coverage`
  pass covered=3, replication status pass databases=3.

The production-bucket lifecycle residual above is unaffected by this
record and still awaits the owner's console read-back.

## Are we doing the bucket backup? Yes — observed state (2026-10-30)

The question "are we doing the bucket backup?" gets asked because the pieces
are documented in different places: this policy states the tiers, the deploy
units enable them, and the gateway reads three JSON sidecars the host
publishes. Nothing before this section answered it in one breath. The answer
is **yes**, and here is the state as observed on the live host, read through
`dg backup status` (which surfaces the same sidecars the TUI's Backups tab
draws):

| Fact | Observed value |
|---|---|
| Target | `s3://dg-sandbox` |
| Endpoint / region | `s3.uk.io.cloud.ovh.net` / `uk` |
| Copying sidecar (`replica.sidecar`) | `active` |
| Retention / snapshot interval | `168h` / `24h` |
| Covered | every live database, plus the registry (`registry: true`) |

The replication service behind that is `dg-litestream.service`, and it reports
`active` on the host. The `replica.json` dataset list names the live databases
(seven at this reading) while the drill and the point-in-time window count the
registry as an eighth database — `metadata.sqlite` is replicated, so both
numbers are right and neither is a discrepancy.

Two more sidecars answer "and is the copy any good?":

- **`drill`** — the last bucket restore drill: `outcome: ok`, eight databases,
  reported as *"restored from the replicas and verified against the live
  gateway"*. A configured bucket proves nothing; a restore that was compared
  against the live gateway does.
- **`restore_points`** — the moments inside the retained WAL window that can be
  restored to, per database, with the oldest and newest file each replica
  holds. At this reading the window opened 2026-10-16T07:30:30Z and the newest
  point was the most recent sync.

### How to check it in one command

Do not go looking for the sidecars by hand. `dg backup status` reads them and
prints the three facts above:

```sh
dg backup status | jq '.replica, .drill, .restore_points'
```

`replica.sidecar` is the field that matters most: a configured replica whose
sidecar is not running is the failure that looks most like success from a
distance. `dg status` does not carry these fields — the backup judgement lives
on `dg backup status` and in the TUI's Backups tab, which is where an operator
should look.

### What this section is not

It is not a new promise. The RPO/RTO objectives above remain the owner's
working target, not a signed production SLA, and the production-bucket
lifecycle residual (the daily-version classes) still awaits the owner's console
read-back. This section records what is running, not what is guaranteed.
