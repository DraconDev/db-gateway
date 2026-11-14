# Resource budget

Pre-production development note. This gateway is not ready for production
use or production data.

Setup is generic: size the host for the OS, the gateway process, and the
replication sidecar your deployment uses; see `docs/resource-harness.md`
for how to run the local harness. This file intentionally contains no
measured host specs, RSS numbers, or dated fit verdicts.

The shipped unit template sets `MemoryMax=64M` in `deploy/dg.service`
as the gateway's cap; `scripts/validate-deploy.sh` holds this page and
the unit to the same value, so change both together.
