# Deployment configuration guide

Template: `deploy/dg.service`. Validation: `scripts/validate-deploy.sh`.
Related: `docs/upgrade-rollback.md` (binary upgrades and rollback),
`docs/observability-contract.md` (monitoring), `docs/recovery-contract.md`
(full host replacement and restore).
Nothing here installs, enables, or starts a service, and this guide is not
live-deploy authorization. The first supervised deploy to micro2 is recorded
separately in `docs/first-deploy-micro2.md`; that record supplies the resolved
values for the installed instance while this guide intentionally retains
placeholders for future/repeated deployments.

## Prerequisites (owner runs on the target)

1. Build the release bundle with `scripts/release-bundle.sh`. The default
   `DG_RELEASE_TARGET=micro2` requires `podman` or `docker` and builds inside
   the immutable `docker.io/library/ubuntu@sha256:496754492fb28b4d3049432f2ca787449331e23fb14f0dd3fffea86bf5a93eb4`
   image (Ubuntu 24.04 amd64, glibc 2.39), matching the recorded micro2 target.
   The container installs the toolchain from `rust-toolchain.toml`, runs the
   locked release build, executes `dg --version` and a disposable `dg init`,
   verifies the x86-64 ELF interpreter, and stages that container-built binary.
   Copy the resulting bundle to the target and verify `SHA256SUMS` there. Use
   `DG_RELEASE_TARGET=host` only for an explicitly local, non-micro2 bundle;
   that path is never labelled micro2 ABI-verified.

   Container selection is automatic: set `DG_CONTAINER_ENGINE=podman` or
   `DG_CONTAINER_ENGINE=docker` to choose explicitly. The micro2 path fails
   closed when neither engine is available, rather than falling back to a
   native build.

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
   The bundle carries the gateway only — no web companion. The console
   runs on the operator machine (`scripts/setup.sh` builds `web/dist`),
   reaching the gateway over an SSH tunnel; `dg web` from a bare bundle
   reports `invalid_console_entry` until a companion is staged beside it.
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

## Preserved properties

- **Loopback listening**: the template defaults to `127.0.0.1:7432`, and the
  server itself rejects non-loopback `--listen` before authentication exists
  (`dg serve --help` documents the option; `validate_listen` enforces it).
- **HTTPS / private transport**: `dg serve` performs no TLS itself because it
  only listens on loopback. The web console client only accepts a loopback
  HTTP origin (web/src/api.ts `GatewayApi` constructor), and the Rust client
  requires HTTPS for any non-loopback endpoint (src/client.rs endpoint
  validation). Remote exposure must therefore terminate TLS at a reverse
  proxy on a private network; plaintext over a non-loopback network is
  refused by the clients, not a supported configuration.
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

`MemoryMax=` is **set** as of 2026-10-10 (`deploy/dg.service`: `MemoryMax=96M`).
The value is 4x the SQLite per-connection cache ceiling (12 connections x
2 MiB, measured from `src/store.rs:36-37` and `src/execution.rs:42`) with
the gateway peak under burst (13 MiB on the dev host, item M2r in
`docs/resource-budget.md`) and the supervised micro2 idle-after-drill
sample (8,292 KiB) as the on-target bounds. Re-run
`scripts/resource-report-release.sh` on the supervised target host to
record the on-target fact; if a later run rejects 96 MiB, lower the cap
in the unit and commit the new report with the change. Items 4 and 5 of
the release-blocking list in `docs/resource-budget.md` remain open.
