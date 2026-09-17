# Architecture and decision register

## Agreed product boundary

DB Gateway (`dg`) serves one owner and database-scoped applications. The server owns validation, authorization, SQL execution, and lifecycle safety; the CLI calls that server rather than implementing separate database behavior.

Target layout:

```text
Owner CLI / application clients
             | HTTPS or private transport
             v
     Rust gateway on micro2
       | independent SQLite files on persistent disk
       v
     SQLite-aware backup component
       | asynchronous upload
       v
     OVH object storage
```

The initial host has approximately 1 GB RAM. A single host is not high availability. Local commit durability does not imply that the latest write has reached the backup bucket.

Non-goals: billing, organizations, SSO, general enterprise roles, web UI, automatic failover, transparent SQLite/Postgres interchangeability, and guaranteed zero-loss host recovery. Later independent-database placement and ephemeral storage are not M0/M1 requirements.

## M0 implementation decisions

- One Rust package in a workspace exposes a `dg` executable and testable library.
- Clap parses commands; Tokio runs the network runtime; Axum supplies routing.
- The only route is `GET /health/live`, returning HTTP 200 and `{"status":"ok"}`. Other routes are absent.
- Listening is restricted to loopback while authentication is absent. No database files, keys, or configuration files are created.
- Structured operational logs go to stderr. CLI parse errors remain Clap's normal usage errors.
- Library/router and binary/argument handling are separated now. Introduce execution, lifecycle, and credential modules with their implementations; empty placeholder modules would not establish real boundaries.

This foundation chooses neither a SQL engine binding nor a client wire protocol.

## Required decisions and evidence

| Decision | Status | Required before |
| --- | --- | --- |
| Custom HTTP client versus required libSQL compatibility | Open: inventory the first real application and its language/features | M1 protocol implementation |
| SQL values, integer/blob encoding, error codes and route schemas | Open; examples from prior discussions are not a contract | M1 protocol implementation |
| Single statements, atomic batches, deadlines and ambiguous writes | Open; no automatic retry of writes without a defined safe policy | M1 writes |
| Idempotency support and retention/resource bounds | Open; either implement a tested contract or explicitly declare it unsupported | M1 writes |
| Recovery objectives (RPO/RTO) | Owner decision pending; no interval or downtime guarantee yet | Backup design |
| OVH compatibility, tool/version, retention and registration lifecycle | Must verify against actual bucket/tool documentation and restore tests | M3 |
| SQLite concurrency, connection/cache/queue/result limits | Establish provisional limits, then measure whole-stack headroom | M1 limits; M4 release gate |
| Host CPU architecture, disk and access transport | Verify actual micro2 configuration with deployment approval | Packaging/deployment |

## M1 design constraints

- Explicit creation: missing databases must never be silently created by a query.
- Owner administration and separate revocable read-only/read/write scoped keys; no SQL endpoint before authorization is in place.
- SQLite controls enforce scope and read-only permissions; SQL prefix matching is insufficient.
- Blocking execution stays off async network threads, with bounded admission, results, and real query cancellation.
- Atomic batches roll back on failure and return clean connections. Cross-database atomicity is not promised.
- A timeout or lost response can leave a committed write's outcome unknown; document this before shipping writes.
- Clone/export/restore use consistent SQLite-aware operations and coordinated lifecycle handling.
- Recovery claims require verified restores and visible backup failures, not merely a configured bucket.

See `TASKS.md` for executable milestones and acceptance conditions. No task completion authorizes deployment, production data changes, or subscription changes.
