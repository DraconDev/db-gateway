# Security

db-gateway is a single-owner, pre-production SQLite service. This file
states the boundary it is deployed inside and how to report a problem
privately. The design rationale lives in `docs/access-edge.md` and the
credential model in `docs/api.md`; this file does not add new claims.

## Supported versions

Only the current release line (`0.3.x`, tagged `v0.3.0`) is supported.
The service is **not production-accepted**: it runs as a supervised
non-production deployment, and no version should be treated as a
production database service yet.

## The deployed boundary

- The gateway binds **loopback only** and refuses public binds. There
  is deliberately no TLS in the gateway itself.
- Remote access rides an authenticated transport the operator already
  trusts: an SSH tunnel, or the WireGuard mesh with socket-activated
  loopback proxies on application hosts (`docs/access-edge.md`). The
  mesh and the tunnel are the transport-security layer.
- Inside that boundary, every request needs a credential: one owner
  key, or database-scoped keys that can only reach their own databases
  (`docs/api.md`). Destructive operations add a typed-name
  confirmation gate.
- Backups leave the host only to the configured OVH S3 bucket
  (snapshot sets plus Litestream replication); credentials for the
  host, the bucket, and the gateway are separate, and none is carried
  in systemd unit `Environment=` lines.

A report that assumes an attacker already inside the loopback/mesh
boundary, or holding a valid key, is describing the trusted side of
this design — but tell us anyway if the consequence surprises you.

## Reporting a vulnerability

Report privately, never as a public issue:

- **GitHub:** open a private report via **Security → Report a
  vulnerability** on `DraconDev/db-gateway` (GitHub private
  vulnerability reporting).
- **GitLab:** open a **confidential issue** on the `DraconDev/db-gateway`
  mirror, marked confidential before describing the problem.

Include the affected version (`dg --version` or the git tag), the
boundary you crossed, and the smallest reproduction you have. This is
a single-owner project: expect an acknowledgement within a few days,
not an SLA. Fixes land on `main`, are noted in `CHANGELOG.md`, and
are deployed under `docs/upgrade-rollback.md`; credit is given in the
changelog unless you ask otherwise.
