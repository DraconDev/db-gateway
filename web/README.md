# Local DB Gateway Console

This package is a workstation-only companion for the DB Gateway `dg` service. It serves a small browser UI on loopback and forwards authenticated requests to an SSH-forwarded `dg` endpoint.

## Run locally

```sh
cd web
npm ci
npm run build
DG_KEY_FILE="$HOME/.config/dg/key" \
DG_SERVER="http://127.0.0.1:17432" \
DG_CONSOLE_PORT=8787 \
node dist/server.js
```

Open `http://127.0.0.1:8787` in a browser. The companion reads the credential file, keeps it in process memory, and injects the bearer token only on requests to `dg`; the browser never receives the owner key. The key file must be a regular file no larger than 4096 bytes and, on Unix, mode `0600` (or stricter).

## SSH tunnel

A managed tunnel can be started by the companion when `DG_SSH_TARGET` is set:

```sh
DG_SSH_TARGET=owner@micro2 \
DG_SSH_LOCAL_PORT=17432 \
DG_SSH_REMOTE_PORT=7432 \
DG_KEY_FILE="$HOME/.config/dg/key" \
node dist/server.js
```

The equivalent explicit command is:

```sh
ssh -N -T -o ExitOnForwardFailure=yes \
  -L 127.0.0.1:17432:127.0.0.1:7432 owner@micro2
```

The console exposes `/api/tunnel/status`, `/api/tunnel/start`, and `/api/tunnel/stop` for explicit lifecycle control. It binds only to loopback, does not deploy a web service, open a public port, or manage remote credentials.

## Verified transfers

Lifecycle exports and imports use the gateway's retained 1 MiB chunk sessions. The browser reports decoded byte progress, verifies the export SHA-256 before offering a download, and retains the session on a transport failure so **Resume** can continue from the server offset. Imports enforce the 128 MiB transfer ceiling before reading the selected file, read upload status before resuming, and expose an explicit **Abort upload** action; a successful commit or an operator abort releases the retained session. Transfer session identifiers and partial data remain in the page/companion process only and are never persisted in browser storage.

The companion validates nested gateway responses at the typed API boundary. Invalid local names, SQL arguments, batch statements, and chunk sizes are returned as structured `invalid_request` errors rather than being converted to an internal server error.

## Truthful visibility

The dashboard shows the gateway's liveness, version/uptime, admission limits, request counters, database inventory, and backup freshness (`fresh`, `stale`, or `unknown`). RSS, disk, CPU, remote-host, OVH/Litestream, alert, and restore-verification signals are intentionally shown as unavailable/not configured until a real collector exists; the console does not infer them from liveness.

## Scope and safety

This is a local operator interface, not a production deployment or a replacement for the CLI. It supports the existing API semantics, including atomic batches, explicit idempotency behavior, `outcome_unknown` errors, scoped keys, and confirmed database deletion. Destructive operations remain owner-only and require the typed-name confirmation in the UI.

All local development and tests should use disposable `dg` data directories and synthetic credentials. No production host, OVH bucket, Turso database, or live data is touched by this package.
