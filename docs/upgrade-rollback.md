# Upgrade / rollback runbook

Scope: replacing the `dg` binary on an existing data directory, and rolling
back to a pre-upgrade snapshot. Local rehearsal only — no target host was
touched. Related: `docs/deployment.md` (install configuration),
`docs/observability-contract.md` (monitoring), `docs/recovery-contract.md`
(full host replacement and restore). Steps that are destructive on a real target carry an explicit
**GATE** and require separate owner approval; the rehearsal below ran only
the safe local equivalents in disposable directories.

## Pre-upgrade snapshot (required, non-destructive)

```sh
DATA_DIR=/var/lib/dg   # target value; rehearsal used a mktemp dir
SERVER=http://127.0.0.1:7432
KEY_FILE=/etc/dg/owner.key   # 0600, owned by the service user

dg --server "$SERVER" --key-file "$KEY_FILE" export <db> --out "<db>.pre-upgrade.sqlite"
sha256sum "<db>.pre-upgrade.sqlite" > "<db>.pre-upgrade.sha256"
# Repeat per database, then a filesystem copy of the stopped data dir
# (see step 2) as the second snapshot leg.
```

Keep both legs: the per-database export (portable, importable anywhere) and
the filesystem copy (byte-exact, fastest restore).

## Upgrade

1. Verify the new bundle: `sha256sum -c SHA256SUMS` in the bundle dir, and
   run `<bundle>/dg --version` — it must print the expected version.
2. **GATE (destructive — stop service):** `systemctl stop dg`. Writes during
   a binary swap risk a torn state; the gateway has no online binary reload.
3. Copy the data dir aside: `cp -a "$DATA_DIR" "$DATA_DIR.pre-<version>"`.
4. Replace the binary at its deployed path; keep the previous binary as
   `dg.prev-<version>`.
5. `systemctl start dg`, then verify: query one known row per database and
   confirm exact values; confirm `dg --version` on the deployed path.
6. Soak: leave the previous binary and the pre-upgrade snapshots in place
   until the owner confirms the new version (retention: until confirmed,
   then delete the `.pre-<version>` copy after the next successful snapshot).

## Rollback

1. **GATE (destructive — stop service, discard post-upgrade writes):**
   `systemctl stop dg`. Restoring the snapshot loses every write made after
   the snapshot; export anything worth keeping first.
2. Restore one leg: either `rm -rf "$DATA_DIR" && cp -a
   "$DATA_DIR.pre-<version>" "$DATA_DIR"` (fast), or re-`init` an empty dir
   and `dg import` each `.pre-upgrade.sqlite` export (portable).
3. Restore the previous binary (`dg.prev-<version>` → deployed path).
4. `systemctl start dg`; verify exact pre-upgrade rows (step 5 above).
5. Record what was lost (post-snapshot writes) and re-apply from application
   sources if needed.

## Notes

- `PRAGMA integrity_check` is rejected over the HTTP API (`sql_forbidden`);
  integrity is verified through exact-row reads and the export/import hash
  chain, not through PRAGMA.
- Idempotency replay state (`_dg_replay`) lives inside each database file,
  so a snapshot restore also restores replay windows — re-sent keys from
  before the snapshot still dedupe; keys issued after it are forgotten.
- Never run upgrade and backup-sidecar reconfiguration in the same window;
  change one thing at a time.

## Appendix: local rehearsal record (2026-09-18)

Disposable dirs under `/tmp`, debug `dg` as "current", release `dg` as the
"upgrade artifact" (both `dg 0.1.0`; same version, different binary).

| Step | Command (essence) | Result |
|---|---|---|
| Baseline | init, create `shop`, insert `widget` | `SELECT *` → `[[1,"widget"]]` |
| Snapshot | `dg export shop --out shop.snapshot.sqlite` + `sha256sum` + `cp -a` | 16,384 bytes, sha `6b0a9adb…19bb`, fs copy 73,728 bytes |
| Upgrade | stop, swap in release binary, restart same data dir | `SELECT *` → exactly `[[1,"widget"]]`; post-upgrade `INSERT gadget` → `rows_affected` 1 |
| Rollback | stop, `rm -rf` + restore fs snapshot, restart original binary | `SELECT *` → exactly `[[1,"widget"]]` (`gadget` gone, as documented data-loss window) |
| Cleanup | kill servers, `rm -rf` rehearsal dir | no `dg serve` processes, dir removed |

All rehearsal servers bound loopback with ephemeral ports; no systemd unit,
no target host, and no destructive action outside `/tmp` was taken.
