# Personal Turso Megaplan

**Status:** Proposed master plan — direction selected, implementation not yet authorized  
**Date:** 2026-09-23  
**Scope:** Turn DB Gateway into a personal, CLI-first hosted database service with local SQLite durability, off-host bucket recovery, and a clear path from one server to a more resilient topology.

This document extends and supersedes the direction in `docs/plan-2026-09-22.md` once adopted. It is a plan, not authorization to touch OVH, production data, subscriptions, or a live host. Every live phase still needs its own approval window.

## 1. Product promise

DB Gateway is the personal Turso replacement:

- The owner uses `dg` from a laptop, terminal, agent, or script.
- The service is hosted on the owner's own server.
- Each application gets a scoped database credential, never the owner key.
- Each database is an independent SQLite file with bounded work and lifecycle operations.
- The server acknowledges a write after the local SQLite transaction commits.
- Verified versioned copies are copied to an off-host OVH Object Storage bucket.
- A new or replacement server can be restored and verified from those copies.
- The CLI remains the primary interface; a web dashboard is not required.

### Important durability distinction

The bucket is a **recovery tier**, not the live query tier.

```text
application write
      |
      v
local SQLite transaction on the primary server  <-- synchronous read/write path
      |
      v
later backup job
      |
      v
verified artifact in OVH Object Storage          <-- asynchronous recovery tier
```

An acknowledged write is locally durable according to SQLite settings. It is not claimed to be in the bucket until a backup reports success and verification. The service must expose that lag rather than pretending that object storage is synchronous.

## 2. Selected product direction

| Area | Selected direction | Consequence |
|---|---|---|
| First topology | Single primary first | Design stable interfaces for later replicas; do not build automatic failover yet |
| Runtime | Native binary under Ubuntu systemd | Keep local SQLite files, logging, and process supervision simple |
| Build consistency | Nix flake for project tooling and checks | Pin development/CI/package inputs without pretending Nix is the Ubuntu ABI boundary |
| Release ABI | Immutable Ubuntu builder image | Build and smoke-test against micro2's Ubuntu/x86_64/glibc contract |
| First bucket target | Verified snapshots | Start with an understandable, testable recovery path; continuous WAL is a later tier |
| Client access | SSH tunnel first | Keep the service private; add HTTPS/private networking when needed |
| Product interface | CLI first | Invest in `dg` commands, JSON output, automation, and shell ergonomics |

The system should remain useful if every optional future feature is removed. That is the test of a good personal service: the core path is small, explicit, and operable without a control plane dashboard.

## 3. Target architecture

```text
                         developer laptop / agent
                                  |
                              dg CLI
                                  |
                       SSH tunnel or private edge
                                  |
                         127.0.0.1:7432
                                  |
                    systemd: dg.service
                                  |
              /var/lib/dg/<name>.sqlite (one file/database)
              /var/lib/dg/metadata.sqlite (registry/auth)
                                  |
             systemd timer: dg-backup.service (one-shot)
                                  |
                 snapshot staging + manifest + hashes
                                  |
                   OVH S3-compatible Object Storage
                                  |
                 separate restore target / replacement host
```

### Runtime services

There are two intentionally different service shapes:

1. **`dg.service` — long-lived gateway daemon**
   - Runs the native `dg` binary.
   - Owns the live SQLite files.
   - Uses systemd restart, logging, resource, and sandbox controls.
   - Is not run inside a container in the first production topology.

2. **`dg-backup.service` — bounded one-shot job, triggered by `dg-backup.timer`**
   - Creates a consistent snapshot set.
   - Uploads only after local validation.
   - Records success/failure and verification metadata.
   - Exits; it is not a continuously running database daemon.

A continuous replication daemon is a future optional component, not part of the first verified-snapshot design.

## 4. Consistency model

Consistency means one source of truth for each concern, not using one tool for everything.

| Concern | Source of truth | How it stays aligned |
|---|---|---|
| Rust compiler | `rust-toolchain.toml` | Flake reads/checks the same pinned channel; release container installs the same version |
| Rust dependencies | `Cargo.lock` | All local, CI, and release builds use `--locked` |
| Local checks | `nix flake check` wrappers plus existing Rust/TS checks | CI runs the flake checks and the repository gates |
| Target ABI | Pinned Ubuntu builder image digest | `release-bundle.sh` refuses native fallback for micro2 |
| Runtime service | `deploy/dg.service` and documented install layout | Systemd validation and install smoke tests |
| Live data | SQLite files under the service data directory | Registry, lifecycle, and backup tools operate through `dg`/store APIs |
| Backup artifact | Manifest, per-file hashes, registry hash, version metadata | Backup and restore validate the same contract |
| Credentials | Owner secret custody and scoped app-key files | Never committed; runtime uses 0600 files or systemd credentials |
| Recovery proof | Dated evidence log and exact-row checks | Restore is not called successful without separate-target verification |

### Why Nix is not the release ABI boundary

Nix is valuable for repeatable development and CI environments, but a Nix-built binary may contain Nix store paths or a different dynamic-linker arrangement. The micro2 release must be built against the target's Ubuntu userland. Therefore:

- flake: project/tooling consistency;
- pinned Ubuntu builder: target compatibility;
- systemd: runtime lifecycle.

These are complementary boundaries, not competing sources of truth.

## 5. Security and ownership model

The service is single-owner, but it is still internet-adjacent infrastructure.

### Runtime boundaries

- Bind `dg` to loopback by default.
- Reach it through an SSH tunnel until an HTTPS/private-network edge is explicitly added.
- Run it as an unprivileged `dg` service account.
- Use `ProtectSystem=strict`, `NoNewPrivileges`, `ProtectHome`, and a narrow `ReadWritePaths` set.
- Keep the data directory private (`0700`) and owned by the service user.
- Keep `/etc/dg/owner.key` at `0600`; the owner's durable copy belongs in secret custody outside the host.
- Give automated applications database-scoped `database_read_only` or `database_read_write` keys.
- Never use the owner key in bots, games, agents, CI, or client libraries.

### Bucket boundaries

- Use a dedicated backup-only bucket credential.
- Scope it to object operations for the backup prefix/bucket only.
- Use TLS for transport and encrypted-at-rest storage.
- Keep bucket credentials out of the repository, unit environment, application database, and logs.
- Prefer systemd credentials or a root-owned 0600 credential file; do not expose secrets to the `dg` process unless required.
- Record bucket region, retention, and budget before live provisioning.

### Threat boundaries

The plan explicitly does not promise:

- multi-tenant isolation;
- billing or abuse controls;
- public internet exposure;
- automatic failover;
- zero-loss host failure;
- transparent SQLite/PostgreSQL interchangeability;
- a hosted control-plane UI.

Those are future products, not accidental consequences of this plan.

## 6. Backup and recovery model

### First production profile: verified snapshots

The first bucket implementation will create a complete, self-describing snapshot set:

```text
snapshot-20260923T193000Z/
  manifest.json
  metadata.sqlite.bak
  <database>.sqlite
  <database>.sha256
```

The manifest records:

- snapshot timestamp and gateway version;
- database names, sizes, and SHA-256 hashes;
- registry presence, size, and hash;
- source host identifier;
- backup completion/verification state.

The backup job must:

1. Enumerate live databases through the gateway's owner API.
2. Export each database through the SQLite-aware export path.
3. Make a consistent registry backup using SQLite online backup, not a raw live WAL copy.
4. Write hashes and manifest to private staging.
5. Validate checksums and database integrity before publication.
6. Upload the complete set to a temporary bucket prefix.
7. Re-read/verify the uploaded object metadata and hashes.
8. Atomically mark the set as complete/verified.
9. Prune only after the new set is verified and retention rules allow it.
10. Record failure, age, and last successful verification visibly.

### Recovery model

Restore always starts on an empty target:

1. Provision a fresh target service user and empty data directory.
2. Download the selected snapshot set.
3. Verify manifest and all hashes.
4. Restore the registry while the target is stopped.
5. Import each database through the validated import path.
6. Run integrity, foreign-key, protected-table, and exact-row checks.
7. Verify scoped application keys against the restored registry.
8. Promote the target only after all checks pass.
9. Keep the previous primary and rollback artifacts until the owner accepts the cutover.

A backup is not considered healthy merely because `upload` returned success. The artifact must be independently verified.

### Snapshot cadence

The first implementation should expose a configurable schedule, but not promise a universal RPO. Recommended starting point:

- one verified snapshot every six hours for active databases;
- daily retention for seven days;
- weekly retention for four weeks;
- explicit exclusions only for deliberately disposable `dev_*`/`scratch_*` databases;
- move toward hourly or continuous WAL only after measured database size, change rate, and bucket cost justify it.

The exact cadence and retention are an owner policy decision at the Phase 0 gate. The implementation must show the real lag and cost rather than silently choosing a reassuring number.

## 7. CLI product plan

The CLI is the primary product surface. It should make the hosted service feel local and scriptable.

### Everyday commands

```sh
dg list
dg create game_a
dg info game_a
dg query game_a "SELECT ..."
dg batch game_a batch.json
dg clone game_a game_a_copy
dg export game_a --out ./snapshot.sqlite
dg import restored --from ./snapshot.sqlite
dg delete game_a_copy --confirm game_a_copy
```

### Operator commands

```sh
dg status
dg backup status
dg backup run
dg backup verify latest
dg restore list
dg restore plan latest
dg restore apply latest --target <host>
```

The last three commands are future work; they should not be added as fake placeholders. Until they exist, the documented restore script is the supported interface.

### Client experience

- JSON by default for non-interactive commands.
- Human-readable table output only for the interactive shell.
- Stable error codes and machine-readable failures.
- Explicit `outcome_unknown` for ambiguous transport failures.
- No hidden retries for writes.
- Scoped credentials for applications.
- Configurable server/key resolution through flags, environment, and a future named-context file.
- Optional named contexts for `micro2`, `staging`, and `local` without putting secrets in shell history.

## 8. Work plan and gates

Every phase has a deliverable, evidence artifact, and rollback boundary. A phase is not complete because code compiles; it is complete when its acceptance evidence is recorded.

### Phase 0 — Product and policy contract

**Goal:** Turn the direction above into explicit owner policy before live infrastructure changes.

Deliverables:

- confirm single-primary-first topology;
- confirm SSH-tunnel-first access;
- confirm verified snapshots as the first bucket profile;
- choose initial cadence, retention, budget, and bucket region;
- document owner-key custody and second-custodian policy;
- document the accepted RPO/RTO objectives and their non-guarantee wording;
- update `docs/recovery-decisions.md` and `docs/deployment-readiness.md`;
- mark the current `docs/plan-2026-09-22.md` as superseded by this plan.

Done when:

- every live-changing value has an owner decision;
- no local implementation relies on an unaccepted provider or retention assumption;
- rollback and restore authority are unambiguous.

### Phase 1 — Reproducible source, package, and release

**Goal:** Make the same source produce a development artifact, CI checks, and a target-compatible Ubuntu bundle.

Deliverables:

- `flake.nix` and `flake.lock` pinning Nix inputs, Rust toolchain, dev shell, checks, formatter, and package metadata;
- a check that the flake and `rust-toolchain.toml` select the same Rust channel;
- a dedicated pinned Ubuntu builder image containing Rust 1.97.1, GCC, libc headers, `pkg-config`, and `readelf`;
- the image pinned by immutable digest, not a floating tag;
- `release-bundle.sh` using the builder for `micro2` with no native fallback;
- bundle `BUILD-INFO.txt` recording image digest, OS, architecture, glibc, engine, and ABI result;
- a deliberate builder-update policy and review checklist;
- SBOM or dependency inventory if the existing release process can support it without blocking the path.

Done when:

- a clean checkout can run the local check suite;
- a release bundle is built inside the pinned image;
- the bundled binary runs `dg --version` and a disposable `dg init` inside that image;
- host mode is clearly marked non-micro2/unverified;
- release tests pass and the exact builder is reproducible from the recorded digest.

Rollback:

- keep the previous native host bundle and previous systemd binary;
- do not deploy a builder change until its bundle and smoke evidence are green.

### Phase 2 — Native daemon packaging and install path

**Goal:** Make the hosted service a boring, repeatable systemd service.

Deliverables:

- hardened `dg.service` for the `dg` user;
- explicit data, key, log, and backup directories;
- install/check/uninstall scripts that do not silently overwrite keys or data;
- `systemd-analyze verify` in CI;
- `dg status` smoke checks after install/restart;
- documentation for SSH tunnel setup and local CLI connection;
- resource limits based on measured values, not guesses.

Done when:

- a disposable host can install the unit from a bundle;
- `systemctl start/restart/stop dg` behaves deterministically;
- restart persistence and exact scoped access survive a restart;
- failed startup produces a clear actionable error;
- the service account cannot write outside its data/backup paths.

Rollback:

- keep the previous binary and data directory snapshot;
- systemd rollback is a binary swap plus `systemctl restart`, with the data directory untouched unless explicitly migrated.

### Phase 3 — Local verified snapshot pipeline

**Goal:** Turn the existing lifecycle operations into a complete local backup artifact before introducing object storage.

Deliverables:

- snapshot staging directory with exclusive ownership;
- registry online backup;
- manifest and per-file checksums;
- atomic publication of a local snapshot set;
- validation of checksums, SQLite integrity, foreign keys, and protected tables;
- backup attempt/success/failure records;
- local retention and cleanup;
- tests for interruption, partial upload, malformed manifest, and stale status.

Done when:

- a write can be followed by a complete local snapshot set;
- an invalid or incomplete set is never presented as healthy;
- repeated backups do not collide or leak staging directories;
- restore from the local set passes exact-row checks.

Rollback:

- manual `dg export` remains available as the emergency fallback;
- do not delete the previous verified set until the new set is complete.

### Phase 4 — OVH bucket adapter

**Goal:** Move verified snapshot sets off-host without making bucket failure look like data success.

Deliverables:

- dedicated backup-only S3 credentials;
- S3-compatible upload with TLS and encryption;
- temporary upload prefix followed by completion marker or atomic manifest publication;
- remote hash/size verification;
- retry with bounded backoff and visible failure;
- retention/pruning driven by verified completion markers;
- bucket outage and permission-failure drills;
- cost/size accounting.

Done when:

- local write → bucket artifact → clean restore target is proven;
- bucket outage leaves local writes serving and marks backup state stale/failed;
- partial uploads are never listed as complete;
- credentials are absent from logs, repository, and application files;
- the measured backup lag is visible through API/CLI.

Rollback:

- stop the uploader while preserving local verified snapshots;
- revoke/rotate the backup credential;
- restore using the last known-good artifact.

### Phase 5 — Recovery, observability, and operator drills

**Goal:** Make recovery a routine, observable operation rather than an emergency guess.

Deliverables:

- restore plan/apply workflow with explicit target selection;
- `dg backup status` extended to last attempt, success, failure, lag, registry inclusion, and restore result;
- disk/inode/RSS/log-retention checks;
- structured incident messages with bounded operator actions;
- daily or scheduled separate-target restore drill;
- documented bucket outage, corrupt artifact, missing registry, and disk-full responses.

Done when:

- a new operator can recover a database from a named snapshot without undocumented knowledge;
- a failed restore publishes nothing to the target;
- the CLI clearly distinguishes `fresh`, `stale`, `failed`, and `unknown`;
- a restore drill produces a committed evidence log.

### Phase 6 — Turso migration

**Goal:** Move real applications without making Turso or the gateway the unquestioned source of truth.

Deliverables:

- read-only source inventory: databases, sizes, schemas, writers, denied SQL, and update rates;
- deny-list census for unsupported SQL/features;
- scratch imports into disposable gateway databases;
- one development-database pilot;
- one low-risk application pilot;
- write-freeze/final-export cutover procedure;
- rollback floor retaining the Turso source until the soak passes;
- application-level row/schema/behavior checks after cutover.

Done when:

- the receiving gateway has verified backup coverage before cutover;
- exact application data and behavior checks pass;
- rollback has been rehearsed, not merely documented;
- Turso remains authoritative until the explicit cutover gate is passed.

### Phase 7 — Scale and resource hardening

**Goal:** Prove the single-primary service supports the intended hundreds-of-databases workload before adding topology.

Deliverables:

- measured resource budget for gateway, backup job, SQLite page cache, and OS;
- benchmark matrix for 1, 10, 100, and target database counts;
- admission/lane tuning based on measurements;
- per-database and global capacity evidence;
- backup lag and upload cost as databases grow;
- explicit decision gate for standby/multi-host work.

Done when:

- the stated client/concurrency target is met with bounded errors;
- resource use leaves documented headroom on the host;
- backup duration and cost are understood at target scale;
- a measured need, not enthusiasm, triggers multi-host design.

### Phase 8 — Optional resilience and product layers

Only after the core path is proven:

- continuous WAL/PITR tier;
- warm standby or multi-host reads;
- automatic failover;
- HTTPS edge or private VPN;
- named CLI contexts;
- web dashboard;
- libSQL-compatible adapter;
- push alerting;
- multi-tenant/billing features.

Each optional layer gets its own proposal, threat model, cost model, rollback, and acceptance evidence.

## 9. Cross-phase invariants

These must remain true in every phase:

1. A query never silently creates a database.
2. A successful write means the local SQLite transaction committed.
3. A bucket upload is not a successful backup until independently verified.
4. An incomplete restore publishes nothing.
5. Raw copies of live WAL databases are forbidden.
6. The owner key never appears in application code, repositories, logs, or automated clients.
7. Scoped credentials remain bound to the correct database generation.
8. The service remains loopback/private unless an access policy is explicitly changed.
9. Release artifacts identify exactly how they were built.
10. Live changes require a bounded, reversible, evidence-producing window.
11. A failed or refused phase leaves the previous primary usable.
12. “Hundreds of databases” means measured capacity, not a marketing claim.

## 10. First execution slice after plan approval

The safest first implementation slice is local and reversible:

1. Add the Nix flake and align its Rust toolchain check with `rust-toolchain.toml`.
2. Add the dedicated builder-image definition and pin its digest.
3. Update the release bundle to use the prebuilt image without `apt-get`/`rustup` network work.
4. Add CI checks that build the image, create a bundle, and inspect `BUILD-INFO.txt`.
5. Update deployment documentation and run the existing release-bundle test.
6. Do not touch the live host, OVH, Turso, or any production data in this slice.

That slice makes the later daemon, backup, and migration phases more trustworthy without making any irreversible infrastructure decision.

## 11. Open policy values

The following values are deliberately not invented here:

- initial snapshot cadence and retention;
- monthly bucket budget and region;
- RPO/RTO acceptance wording;
- owner-key second custodian;
- target concurrent-client count;
- whether/when a standby is justified.

They belong in Phase 0 owner decisions. The implementation should expose real measurements and make the cost of each choice visible.

## 12. Definition of the finished personal service

The first meaningful “personal Turso” milestone is reached when:

- an owner can point `dg` at the hosted service and manage databases from the CLI;
- applications use scoped keys and cannot cross database boundaries;
- writes survive gateway and host process restarts;
- a verified bucket snapshot exists after a normal write;
- a separate target can restore that snapshot and pass exact-row checks;
- the operator can tell whether backup is fresh, stale, failed, or unknown;
- a Turso database can be surveyed, trial-imported, piloted, and cut over with rollback evidence;
- a release can be rebuilt from the pinned source/tooling and target builder contract;
- the whole path is operable with one CLI, one service account, and documented scripts.

That is the target. Everything beyond it is optional expansion, not a prerequisite for a useful personal database service.
