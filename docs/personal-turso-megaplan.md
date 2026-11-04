# DB Gateway overview

DB Gateway is a personal, single-owner SQLite database service with a Rust
gateway (`dg`) and CLI. This is pre-production development; it is not ready
for production use or production data.

Setup is generic: run `dg serve` on a Linux host with persistent local
storage, configure scoped keys and a private transport, and see
`docs/deployment.md` for the deploy procedure. This file intentionally
contains no internal roadmap, milestone status, or migration plan.
