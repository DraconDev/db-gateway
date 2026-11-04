# Security

db-gateway is a single-owner, pre-production SQLite service. This file
states the boundary it operates inside and how to report a problem
privately. The design rationale lives in `docs/access-edge.md` and the
credential model in `docs/api.md`; this file does not add new claims.

## Supported versions

Only the current release line (`0.10.x`, tagged `v0.10.0`) is supported.
The service is pre-production; no version should be treated as a production database service yet.

## The deployed boundary

- The service is intended for a private trusted boundary, not the open internet.
- Any remote access must be mediated by an authenticated private path configured by the operator.
- Inside that boundary, requests require credentials; per-database credentials are limited to their own databases (`docs/api.md`). Destructive operations add a typed-name confirmation gate.
- Backups are operator-configured off-host copies; credentials for the host, the backup destination, and the gateway are separate.

A report that assumes an attacker already inside the trusted boundary, or holding a valid key, is describing the trusted side of this design — but tell us anyway if the consequence surprises you.

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
