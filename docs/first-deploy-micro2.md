# First deploy record — micro2 (2026-09-18, supervised, owner-approved)

## Authorization (inspectable)

- Goal item required explicit owner go-ahead before any host contact.
- Executor paused with a decision card (`pauseKind: decision`, ledger
  `.pi-glla/active.jsonl` `2026-09-18T13:50:26Z`) offering deploy / queue /
  drop; the owner selected **option 1 (approve the supervised deploy)**
  in chat, then supplied SSH details by pointing at the local
  `~/.dracon` ssh config (`ubuntu@145.241.238.20`, `~/.ssh/micro2.key`).
- Goal re-activated after approval (ledger `2026-09-18T14:02:42Z`,
  resumed `2026-09-18T14:07:13Z`); first host contact was a read-only
  survey. No host was touched before the approval.

## Verbatim host evidence

`docs/first-deploy-micro2-evidence.log` (42 lines, committed alongside
this record) is the unedited stdout of a re-verification pass run on
the host over SSH: `dg --version`, `sha256sum -c SHA256SUMS`, installed
binary hash match, `is-active`/`is-enabled`, pre/post-restart SELECTs,
a full export/import drill on a throwaway pair (created, verified,
deleted), and final state. It contains no key material (key file is
referenced by path only).

Target: `micro2`, Ubuntu (kernel `6.17.0-1019-oracle`, x86_64), 2 vCPU,
~1 GiB RAM, 25 GiB free on `/`. glibc 2.39. Access over SSH as `ubuntu`
(passwordless sudo present); gateway itself listens on loopback only.

Layout (all values resolved):

| Placeholder      | Value                |
|---|---|
| `{{DG_USER}}`    | `dg` (system user, nologin, home `/var/lib/dg`) |
| `{{DG_BINARY}}`  | `/usr/local/bin/dg` (root-owned, `0755`, `dg 0.1.0`) |
| `{{DG_DATA_DIR}}`| `/var/lib/dg` (`dg:dg`, `0700`, 57344 bytes after drill) |
| `{{DG_LISTEN}}`  | `127.0.0.1:7432` |

Owner credential: `dg init` run once as `dg`; raw 64-hex key extracted
from the printed JSON into `/etc/dg/owner.key` (`dg:dg`, `0600`).
Unit: template rendered with zero remaining placeholders,
`systemd-analyze verify` clean, daemon reloaded, service **enabled**
and **active**.

## Procedure evidence (in order)

1. Bundle built via `scripts/release-bundle.sh`, `SHA256SUMS` verified
   **on the host** (`sha256sum -c`: all files `OK`).
2. Smoke: `create deploy_smoke` → `{"name":"deploy_smoke"}`;
   DDL + INSERT + `SELECT` → exact `[[1,"smoke-1"]]`.
3. Restart survival: `systemctl restart dg.service` → `active`;
   re-query → exact `[[1,"smoke-1"]]` (no loss, no duplication).
4. Restore drill: `export deploy_smoke --out <snapshot>` (16384 bytes,
   sha `d5bc0d88…946d`) → `import deploy_restored --from <snapshot>` →
   `SELECT` on the copy exact `[[1,"smoke-1"]]`, source unchanged.
5. Copy-independence: INSERT into the copy only → source stays
   `[[1,"smoke-1"]]`, copy reads `[[1,"smoke-1"],[2,"restored-only"]]`.
6. Baseline on host: service `VmRSS` 8292 KiB idle-after-drill.

## Build note (portability)

The local toolchain links a Nix-store absolute interpreter path, which
does not exist on Ubuntu. `scripts/release-bundle.sh` now defaults to
`DG_RELEASE_TARGET=micro2`, adds
`-C link-args=-Wl,-dynamic-linker,/lib64/ld-linux-x86-64.so.2`, and verifies
both the x86_64 ELF machine and interpreter before staging. The staged
bundle binary was confirmed to carry `/lib64/ld-linux-x86-64.so.2` before
transfer (max required symbol `GLIBC_2.38`, host provides 2.39). A first
transfer without the flag failed on-host with `cannot execute: required file
not found` despite `sha256sum -c` passing — checksum validity does not imply
executability.

## Deploy-guide correction applied

`dg init` prints JSON (`{"owner_key":"…"}`), not the raw credential, so
step 5 of `docs/deployment.md` now shows the extraction; the key file
must contain exactly the 64 hex characters (`read_key` rejects anything
else client-side with `invalid_request` before any request is sent).
The 0600-mode check is enforced by the CLI, not documented folklore.

## Left for later (not this deploy)

- `MemoryMax=` stays commented (single 8 MiB idle sample is not a
  measured budget; see `docs/resource-budget.md` items 1–5).
- No backup sidecar, no TLS terminator, no monitoring; loopback-only
  access, SSH tunnel for remote CLI per gate 3.
- No secrets in this record; the owner key exists only on micro2.
