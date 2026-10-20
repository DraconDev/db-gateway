# Changelog

All notable changes to db-gateway are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Release practice.** One version identifies the whole project: the
workspace `Cargo.toml`, `crates/dg-client/Cargo.toml`, and the root /
`client/` / `web/` `package.json` files move together in a single bump
commit (lockfiles refreshed in the same commit: `cargo update -w`, and
the npm lockfiles' root entries). The release commit is tagged
`vX.Y.Z` (annotated) — the tag, `dg --version`, and the version
`scripts/release-bundle.sh` stamps into every release bundle are the
same number, so a deployed bundle can always be traced to exactly one
tagged state. Deployments and rollbacks follow
`docs/upgrade-rollback.md`. While the project is pre-production
(0.x), minor versions mark feature growth and patch versions mark
fixes; 1.0.0 waits for production acceptance.

## [Unreleased]

## [0.2.0] — 2026-10-19

The first release-identified state: the feature set that has been
running as a supervised non-production deployment on micro2, plus the
2026-10-19 audit hardening. Production is **not** accepted; see
`README.md` for the live status.

### Added

- Authenticated SQLite gateway: owner bootstrap, database-scoped API
  keys, SQL query/batch over HTTP with idempotent writes, interactive
  shell, JSON CLI, and a workstation web console.
- Database lifecycle: clone, export/import (validated, atomic), and
  tombstone delete with a typed-name confirmation gate.
- Backups: hourly verified snapshot sets, continuous Litestream
  replication to OVH S3, backup-health status/digest, and restore
  drills wired to systemd timers.
- Access edge: loopback-only service reached via SSH tunnel or the
  WireGuard mesh with socket-activated proxies; public binds refused.
- Observability: request-pressure counters, backup-freshness signals,
  and alert notices under `docs/observability-contract.md`.
- Release identity: this changelog, the `vX.Y.Z` tag practice, and
  `SECURITY.md`. Every tracked script is lint-gated in CI
  (`scripts/lint-scripts.sh`), and the docs tree is indexed
  (`docs/README.md`) with dated evidence under `docs/evidence/`.

### Changed

- The deployed micro2 binary still reports `dg 0.1.0` until the next
  upgrade window; 0.2.0 is the repository state that window will
  deploy.
- **The CLI now draws its terminal output through one shared renderer**
  (`src/render.rs`): box-drawn tables with numbers right-aligned under
  their headers, a `NULL` that is distinct from the text `"NULL"`,
  humanised byte counts, durations, epochs and backup-set stamps, column
  widths measured by display width so wide characters do not shear the
  grid, and colour as decoration only (`NO_COLOR` turns it off; every
  coloured state is also spelled out in words). `dg status` and
  `dg backup status` became dashboards (titled sections for service,
  admission, requests and per-database state) instead of one long
  `property | value` list. Piped output and `--json` remain
  byte-identical JSON, so scripts are unaffected.

## [0.1.0] — 2026-09-18

First supervised deployment to micro2 (retroactive entry, recorded
2026-10-19 when this changelog was adopted): the authenticated gateway
and CLI from milestones M1–M3, before the audit hardening above.
