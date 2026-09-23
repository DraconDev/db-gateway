# Deployment configuration guide

Template: `deploy/dg.service`. Validation: `scripts/validate-deploy.sh`.
Nothing here installs, enables, or starts a service, and this guide is not
live-deploy authorization. The first supervised deploy to micro2 is recorded
separately in `docs/first-deploy-micro2.md`; that record supplies the resolved
values for the installed instance while this guide intentionally retains
placeholders for future/repeated deployments.

## Prerequisites (owner runs on the target)

1. Build the release bundle with `scripts/release-bundle.sh` (the default
   `DG_RELEASE_TARGET=micro2` verifies x86_64 Linux and the
   `/lib64/ld-linux-x86-64.so.2` interpreter), copy it to the target, and
   verify `SHA256SUMS` there. Use `DG_RELEASE_TARGET=host` only for an
   explicitly local, non-micro2 bundle.
2. Create the dedicated unprivileged user (see placeholder table).
3. Place the `dg` binary at its final path, owned by root, executable.
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

Beyond the placeholder table: the service username, binary/data paths, the
`MemoryMax=` value (pending `docs/resource-budget.md` items 1-5 — the line
stays commented until measured), log retention for the JSON stderr stream,
and the backup sidecar unit (Litestream — see `docs/backup-compatibility.md`;
no unit is provided here).
