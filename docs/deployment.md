# Deployment configuration guide

Template: `deploy/dg.service`. Validation: `scripts/validate-deploy.sh`.
Related: `docs/upgrade-rollback.md` (binary upgrades and rollback),
`docs/observability-contract.md` (monitoring), `docs/recovery-contract.md`
(full host replacement and restore).
Nothing here installs, enables, or starts a service, and this guide is not
live-deploy authorization. The first supervised deploy to micro2 is recorded
separately in `docs/evidence/first-deploy-micro2.md`; that record supplies the resolved
values for the installed instance while this guide intentionally retains
placeholders for future/repeated deployments.

## Prerequisites (owner runs on the target)

1. Build the release bundle with `scripts/release-bundle.sh`. The default
   `DG_RELEASE_TARGET=micro2` requires `podman` or `docker` and builds inside
   the immutable `docker.io/library/ubuntu@sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4`
   image (Ubuntu 24.04 amd64, glibc 2.39), matching the recorded micro2 target.
   The container installs the toolchain from `rust-toolchain.toml`, runs the
   locked release build, executes `dg --version` and a disposable `dg init`,
   verifies the x86-64 ELF interpreter is `/lib64/ld-linux-x86-64.so.2`, and
   refuses a `DG_RELEASE_TARGET=micro2` bundle whose glibc requirement exceeds
   `EXPECTED_GLIBC` (`scripts/release-bundle.sh`; currently 2.39), stages that
   container-built binary.
   Copy the resulting bundle to the target and verify `SHA256SUMS` there. Use
   `DG_RELEASE_TARGET=host` only for an explicitly local, non-micro2 bundle;
   that path is never labelled micro2 ABI-verified.

   Nominate a built bundle for the deploy-time guard with
   `DG_RELEASE_BUNDLE_DIR=<bundle-dir> scripts/validate-deploy.sh`. The guard
   inspects the nominated `dg`: it re-verifies that bundle's `SHA256SUMS`, then
   checks the ELF's interpreter, that neither `DT_RPATH` (tag 15) nor
   `DT_RUNPATH` (tag 29) resolves from `/nix/store`, and that the glibc ceiling
   recorded in `DT_VERNEED` does not exceed `EXPECTED_GLIBC`. Without a
   nomination it falls back to the local build, which fails on a NixOS
   workstation precisely because a Nix build cannot be deployed to Ubuntu.

   Container selection is automatic: set `DG_CONTAINER_ENGINE=podman` or
   `DG_CONTAINER_ENGINE=docker` to choose explicitly. The micro2 path fails
   closed when neither engine is available, rather than falling back to a
   native build.

   The same gate checks the documents against the unit: the gateway's
   memory cap is stated in `deploy/dg.service`, and every live document
   that names the unit must state the same cap
   (`scripts/check-doc-caps.py`, run by the gate). The sidecar's own cap,
   the sandbox's 1 GiB scope, the harness's measured suggestion and the
   dated records under `docs/evidence/` are not the gateway's cap and are
   left alone. The check exists because a document that states a
   different ceiling contradicts the file that is actually installed,
   and the operator reads the document.

   **Builder update policy:** treat the image digest and expected glibc family
   as release inputs. Update them only for an intentional micro2 OS/glibc
   change: first record the target's architecture and `ldd --version` output,
   then select the matching Ubuntu image digest, run
   `DG_RELEASE_TARGET=micro2 scripts/release-bundle.sh` plus
   `cargo test --locked --test release_bundle`, review `BUILD-INFO.txt`, and
   commit the image/expected-glibc change together. Never substitute a
   floating tag, newer glibc, or a locally built image without updating and
   testing the pinned contract.
2. Create the dedicated unprivileged user (see placeholder table).
3. Place the `dg` binary at its final path, owned by root, executable.
   The bundle carries the gateway only. The operator's terminal view
   (`dg tui`) runs on the operator machine and reaches the gateway over
   an SSH tunnel, so nothing but the binary is needed on the host.
4. Create the data directory, owned by the service user, mode `0700`.
5. Run `dg init --data-dir <dir>` once as the service user. It prints
   JSON (`{"owner_key":"…"}`): extract the raw 64-hex value into the
   `0600` file (e.g. `umask 077; dg init … | python3 -c 'import json,sys;
   print(json.load(sys.stdin)["owner_key"])' > owner.key; chmod 600 owner.key`).
   Verify the mode with `stat -c '%a %U:%G' owner.key` before use. The CLI
   rejects anything but exactly 64 hex characters client-side.
6. Copy the template to `/etc/systemd/system/dg.service` with placeholders
   substituted, `systemctl daemon-reload`, then enable/start explicitly.

## Placeholder values

| Placeholder | Meaning | Status |
|---|---|---|
| `{{DG_USER}}` | Dedicated unprivileged service user | **Unresolved** — target-host decision (e.g. `dg`) |
| `{{DG_BINARY}}` | Absolute path of the `dg` executable | **Unresolved** — target layout decision (e.g. `/usr/local/bin/dg`) |
| `{{DG_DATA_DIR}}` | Persistent data directory | **Unresolved** — target layout decision (e.g. `/var/lib/dg`) |
| `{{DG_LISTEN}}` | Listen address; must stay loopback | Default `127.0.0.1:7432`; any change must remain a loopback address or `dg serve` refuses to start |
| `{{DG_BACKUP_ROOT}}` | Versioned backup-set root the hourly producer publishes into (also `dg-backup.service`/`dg-backup-digest.service` `WorkingDirectory=`) | **Unresolved** — target layout decision (e.g. `/var/lib/dg-backups`) |
| `{{DG_BACKUP_SCRIPT}}` | Absolute path of the installed hourly backup producer (`scripts/micro2-backup.sh` on the reference host) | **Unresolved** — target layout decision (e.g. `/usr/local/bin/micro2-backup.sh`) |
| `{{DG_RESTORE_DRILL_SCRIPT}}` | Absolute path of the installed weekly bucket restore drill (`scripts/bucket-restore-drill.sh`) | **Unresolved** — target layout decision (e.g. `/usr/local/bin/bucket-restore-drill.sh`) |
| `{{DG_OWNER_KEY_FILE}}` | Absolute path of the mode-0600 owner credential file the hourly producer and the restore drill read (never its contents) | **Unresolved** — target layout decision (e.g. `/etc/dg/owner.key`) |
| `{{DG_RESOURCE_SAMPLE_SCRIPT}}` | Absolute path of the installed composed resource sampler (`scripts/resource-sample.sh`) | **Unresolved** — target layout decision (e.g. `/usr/local/bin/resource-sample.sh`) |
| `{{DG_ALERTS_SCRIPT}}` | Absolute path of the installed disk/lag alert checker (`scripts/dg-alerts.sh`) | **Unresolved** — target layout decision (e.g. `/usr/local/bin/dg-alerts.sh`) |
| `{{DG_MESH_LISTEN}}` | WireGuard interface `address:port` the mesh-edge socket binds (`deploy/dg-mesh-proxy.socket`) | **Owner decision at edge install** — must be the host's mesh address (e.g. `10.66.0.1:7432`); never a wildcard/LAN/WAN address (`docs/access-edge.md`) |

The backup units (`deploy/dg-backup.{service,timer}` hourly producer,
`deploy/dg-backup-digest.{service,timer}` daily digest) are templates in
the same style: substitute the placeholders above, install under
`/etc/systemd/system/`, `daemon-reload`, then enable the timers.

`dg-backup.service` passes the rendered paths to the producer it drives
(`--server http://{{DG_LISTEN}} --key-file {{DG_OWNER_KEY_FILE}}
--data-dir {{DG_DATA_DIR}} --backup-root {{DG_BACKUP_ROOT}}`), and
`dg-backup-digest.service` reads the same root. The producer's own
defaults are the reference layout; a host that renders these templates
with a different layout must have the unit pass the substituted values,
or the hourly producer publishes into `/var/lib/dg-backups` while the
daily digest reads the configured root.
`scripts/host-audit.sh` compares the installed units against these
templates (rendered with the reference-host values, comments/blank
lines ignored) and fails on drift, so a hand edit on the host cannot
silently diverge from the repo. The render is strict: a template that
gains a placeholder fails the check, naming it, until the host's value
for that placeholder is stated. A template rendered with the literal
`{{...}}` text still in it would hash to a value no correctly
rendered host has, so the check would report a permanent false drift on
every healthy host — and a permanently red check cannot find the real
drift it exists to detect. `scripts/unit-render.sh` is that render,
and `scripts/validate-deploy.sh` uses it too, so the deploy gate
verifies units that carry no template text.

**Non-loopback access (audit A9):** application hosts reach the
gateway over a WireGuard mesh via `deploy/dg-mesh-proxy.{socket,service}`
(socket-proxyd to loopback; the gateway itself never leaves loopback).
Decision, threat model, and rejected alternatives:
`docs/access-edge.md`. Not installed anywhere yet — edge install is
a separate owner-approved change.

**Alerts and journal retention (audit A8):** `deploy/dg-alerts.{service,timer}`
runs the disk-headroom + Litestream-lag checker every 15 minutes
(structured `dg-alert` journal lines + nonzero exit on alert;
delivery is the journal, per `docs/observability-contract.md` §3).
`deploy/journald-10-dg.conf` is the journald drop-in template
(install at `/etc/systemd/journald.conf.d/10-dg.conf`, restart
`systemd-journald`): SystemMaxUse=128M, SystemKeepFree=2G,
MaxRetentionSec=14day — sized so dg journal evidence survives on
the 1 GB host instead of being vacuumed by defaults.

**Digest canonical mechanism:** on any systemd deployment the daily
digest is the external `dg-backup-digest.timer` above, and
`dg.service` must keep `--backup-digest-secs 0` (off, the default) —
the in-server digest flags are only for non-systemd single-process
runs. Running both double-delivers and lets each mask the other's
failure (`docs/observability-contract.md` §3a-0).

## Preserved properties

- **Loopback listening**: the template defaults to `127.0.0.1:7432`, and the
  server itself rejects non-loopback `--listen` before authentication exists
  (`dg serve --help` documents the option; `validate_listen` enforces it).
- **HTTPS / private transport**: `dg serve` performs no TLS itself because it
  only listens on loopback. The Rust client requires HTTPS for any
  non-loopback endpoint (src/client.rs endpoint validation), and the
  terminal view uses that same client. Remote exposure must therefore
  terminate TLS at a reverse proxy on a private network; plaintext over a
  non-loopback network is refused by the clients, not a supported
  configuration.
- **Dedicated unprivileged execution**: `User=`/`Group=` plus
  `NoNewPrivileges=true`, `ProtectSystem=strict`, `ProtectHome=true`.
- **Persistent data ownership**: the data dir is the working directory and
  the only writable path (`ReadWritePaths`); it must be owner-persistent
  across restarts and upgrades.
- **Restart policy**: `Restart=on-failure` with a 5 s backoff; clean exits
  (e.g. operator `systemctl stop`) are not restarted.
- **Secure credential handling**: no secrets in the unit, environment, or
  flags. Owner bootstrap via `dg init` (stdout redirected securely); client
  keys via `0600 --key-file` files. Key files live outside the data dir
  backup set.

## Unresolved target-host values

Beyond the placeholder table: the service username, binary/data paths, log
retention for the JSON stderr stream, and the backup sidecar unit
(Litestream — see `docs/backup-compatibility.md`; no unit is provided here).

`MemoryMax=` is **set** as of 2026-10-10, revised to the on-host value 2026-10-16 (`deploy/dg.service`: `MemoryMax=64M`, matching the micro2 enforcement of 67108864 recorded in `docs/evidence/m4-micro2-sampling-2026-10-16.log`).
The value is ~2.7x the SQLite per-connection cache ceiling (12 connections x
2 MiB, measured from `src/store/admission.rs:12-14` and `src/execution.rs:42`) with
the gateway peak under burst (11 MiB under a 1 GiB-enforced local harness,
item M6 in `docs/resource-budget.md`) and the on-target steady sample
(6696 KiB) as the bounds. Target-host burst at the 429 edge and disk growth,
the release-blocking list in `docs/resource-budget.md`, remain open.

## Host verification (`scripts/host-audit.sh`)

`scripts/host-audit.sh` is the read-only gate that proves a deployed host is
in the state the repo describes. It emits one JSON object on stdout and
exits non-zero when any invariant fails; diagnostics go to stderr.

### Modes

Set `DG_AUDIT_TARGET` to an ssh-config alias or `user@host` (never an IP or
key path in the repo — credentials resolve from `~/.ssh/config` and the SSH
agent). Set `DG_RELEASE_BUNDLE_DIR` to a `scripts/release-bundle.sh` output
directory to nominate the exact binary the host is held to.

- **No `DG_AUDIT_TARGET`** — repo-side only. Re-reads the deployed backup
  script from `git HEAD:`, `bash -n`'s it, scrapes its path constants
  (`BACKUP_ROOT` / `KEY_FILE` / `DATA_DIR`), and nominates the bundle digest.
  `host_side.status` is `"skipped"` and the script exits 0. This is what CI
  runs (`.github/workflows/ci.yml`, alongside `validate-deploy.sh`).
- **`DG_AUDIT_TARGET` set, probe cannot run** — `host_side.status` is
  `"unreachable"` and the script exits non-zero, naming the ssh failure.
  This is the path that closes the "Permission denied" hole where a detached
  audit could not reach the host and had to trust ad-hoc commands.
- **`DG_AUDIT_TARGET` set, all probes answer** — `host_side.status` is
  `"pass"` or `"fail"`; any failed check flips `verdict` to `"fail"` and the
  exit code to non-zero.

### What the live probe proves

Binary sha256 matching the nominated bundle's `SHA256SUMS` (plus mode and
owner), `dg.service` and `dg-backup.timer` `ActiveState` with `User=dg`,
`/usr/local/bin/micro2-backup.sh` sha256 matching `git HEAD:`, zero stale
`.staging*` directories under the backup root, the newest versioned set's
manifest age under `DG_AUDIT_MAX_SET_AGE_HOURS` (default 26h — one hour
past the hourly timer), and `dg backup verify latest` returning `ok:true`.
The owner-key file's mode and ownership are recorded from `stat` only; the
key bytes are never read.

Anti-staleness: `is-active` alone is never sufficient. The probe closes the
2026-09-22 outage shape where the timer reported active while no real
backup had published for three weeks.

### Cross-checking a pasted record

An operator runs the live mode and pastes the JSON into the audit record.
To verify a pasted record:

```sh
DG_AUDIT_TARGET=<alias> DG_RELEASE_BUNDLE_DIR=<bundle> \
  scripts/host-audit.sh | tee record.json; echo "exit=$?"
jq -e '.mode=="live" and .exit_code==0 and .verdict=="pass"
       and ([.repo_side[]|.status]|all(.=="pass" or .=="observed"))
       and .host_side.status=="pass"
       and ([.host_side.checks[]|.status]|all(.=="pass"))
       and .host_side.checks.dg_binary_sha256.expected
         == .host_side.checks.dg_binary_sha256.actual
       and .host_side.checks.stale_staging_dirs.count==0
       and .host_side.checks.backup_verify.report.ok==true' record.json
```

`exit_code` is embedded in the report so the pasted record proves the
script's own exit status. A record showing `"status": "skipped"` inside
`host_side.checks` for a live run means that probe step found nothing to
check (not that it passed); `owner_key_mode` is the only routinely-skipped
check and it is skipped only when the key file is absent.

## Off-host replication (`dg-litestream.service`)

The same-disk backup root protects against logical loss only. A disk
failure takes the databases and their backups together. `dg-litestream`
is the second leg: a systemd service that replicates every live `.sqlite`
under `/var/lib/dg/` to the recorded object store using
litestream 0.5.17, with its credentials in `/etc/dg/litestream.env` (mode
0600, owner `dg:dg`) — the only copy on the host — and nothing in the
unit, the config, or the repository carrying a secret (the config uses
`${LITESTREAM_S3_*}` env-var references populated by systemd's
`EnvironmentFile=`).

### Installing

```sh
# 1. The operator (or the bucket-creation runbook) writes the credential
#    file once. scripts/install-litestream.sh refuses to run without it,
#    and never reads or echoes its contents:
#    /etc/dg/litestream.env, mode 0600, owner dg:dg, containing:
#      LITESTREAM_S3_BUCKET=...
#      LITESTREAM_S3_ENDPOINT=...
#      LITESTREAM_S3_REGION=...
#      LITESTREAM_S3_ACCESS_KEY_ID=...
#      LITESTREAM_S3_SECRET_ACCESS_KEY=...
scripts/install-litestream.sh        # installs, renders config, drops units
ssh micro2 'sudo systemctl enable --now dg-litestream dg-litestream-sync.timer'
ssh micro2 'sudo journalctl -u dg-litestream -n 50 --no-pager'
```

The install script verifies the litestream tarball's SHA-256 against the
upstream `checksums.txt` for the pinned release tag before installing,
and is idempotent — a re-run detects the installed binary and reuses it.
Nothing is started automatically: the operator chooses when the sidecar
first begins replicating.

The config is rendered by `scripts/litestream-sync.sh` (installed as
`/usr/local/bin/litestream-sync.sh`): litestream reads its config only
at startup, so the `dg-litestream-sync.timer` re-renders from `dg list`
every five minutes and restarts the sidecar only when the config
changed — a database created or deleted after install enters or leaves
replication within one interval, with no operator step
(docs/audit-2026-10-19.md A2). `litestream-sync.sh --check` reports
per-database coverage, and host-audit's `litestream_coverage` check
fails if any live database is missing from the config. Local proof:
`scripts/litestream-sync-drill.sh`
(evidence: docs/evidence/litestream-sync-drill-2026-10-19.log).

### Proving a restore round-trip

```sh
scripts/litestream-restore.sh
```

For every live database the script runs `litestream restore` into a
0700 temp directory, asserts `PRAGMA integrity_check` is `ok`, enumerates
every user table, and asserts each `count(*)` matches what the live
gateway is serving. Non-zero exit on the first mismatch. The same
comparison logic is pinned by `tests/litestream_restore.rs`, which runs
against a synthetic mirror pair so the contract clause — "row counts plus
PRAGMA integrity_check match the live sources exactly" — is checkable
without a bucket.
