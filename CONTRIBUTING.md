# Working on db-gateway

How development on this repo is supposed to flow — for the owner and for
any agent working with them. This is the short version; the documents it
points at are the binding ones.

**The model in one paragraph:** this is a single-owner, pre-production
project. All work lands on `main` (no feature branches), one backlog item
at a time, and an item is only "done" when its gates pass *and* its
closure is written down. Nothing live — deploys, drills against real
infrastructure, production-data actions — happens without the owner's
separate, explicit approval. The service is **not production-accepted**;
do not deploy it as a production database service.

## Where the truth lives

| Question | Document |
| --- | --- |
| Where is the project going, and why? | `docs/personal-turso-megaplan.md` (canonical roadmap + decision register) |
| What is being built right now? | `TASKS.md` (live implementation checklist) |
| What is broken, missing, or risky? | `docs/audit-2026-10-19.md` (the audit backlog; items A1… close one at a time) |
| Which docs can I trust? | `docs/README.md` — every doc is classed canonical, superseded, or historical evidence |
| What changed in each release? | `CHANGELOG.md` |
| How do I report a vulnerability? | `SECURITY.md` (privately, never a public issue) |
| What does CI enforce? | `.github/workflows/ci.yml`, `.github/workflows/lint-scripts.yml`, policy in `deny.toml` |

If two documents disagree, the megaplan wins for direction and the code
plus tests win for behaviour — then fix the stale document in the same
change.

## The loop for one item

1. **Pick** the next item (usually the next open entry in the audit
   backlog, or one the owner names). The item's *Done when* line is the
   contract; if it is ambiguous, sharpen it before starting.
2. **Break it down** into a task list when it is more than a one-file
   change, and track progress against that list as you go — not as a
   post-hoc checklist at the end.
3. **Implement on `main`.** Prefer the durable, root-cause fix over the
   cosmetic patch. If the durable fix is genuinely blocked, record why
   and what the follow-up is; never silently downgrade the goal.
4. **Run the gates** (below) on the final tree. A gate that fails gets
   fixed or the item stays open — gates are never weakened, skipped, or
   re-worded to make a run green.
5. **Write the closure** in the item's home document (for audit items:
   flip `- [ ]` to `- [x]` in `docs/audit-2026-10-19.md` with a dated
   paragraph saying what changed and what proved it). Transcripts and
   dated records under `docs/evidence/` are never rewritten — a
   correction is a new dated note, not an edit to history.
6. **Independent check.** For agent-driven work, a fresh reviewer (or
   the completion auditor) rehearses the item's *Done when* contract
   against the actual files and command output before the item is
   called done. Claims of "tests pass" must name the command and the
   result, and the exact spelling that CI runs must be the spelling
   that was verified locally.

## The gates

Run these on the tree you are about to call done (CI runs the same set):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked            # 218 tests; ~260s execution
cargo deny check                           # licenses / advisories / bans / sources
npm audit --prefix client --audit-level=high
npm audit --prefix web --audit-level=high
npm test                                   # root script: client suite (real dg serve) + web console suite/E2E
./scripts/lint-scripts.sh                  # every tracked script, shellcheck-warning clean
python3 scripts/check_plan_consistency.py docs/personal-turso-megaplan.md
./scripts/validate-deploy.sh               # when deploy/ templates change
```

CI (`.github/workflows/ci.yml`) runs three jobs — `check` (the Rust and
npm gates above, plus deploy-template and host-audit checks), `deny`
(the cargo-deny policy), and `audit` (RustSec) — and
`lint-scripts.yml` gates script changes. Two standing rules:

- **Suite-time budget: 600s** for test-binary execution in CI
  (measured 261s on 2026-10-19). Exceeding it warns; it does not fail.
  Raise the budget only with a fresh measurement recorded in `ci.yml`
  and the audit doc — never just to silence the warning.
- **Dependency policy:** a new dependency whose license is outside the
  `deny.toml` allowlist, a wildcard version requirement, or a
  non-crates.io source fails the gate on purpose. The fix is a
  deliberate `deny.toml` edit with a reason, not a bypass.

## Documentation rules

- **Canonical** docs are edited in place, and meaning changes are
  propagated to the docs that depend on them.
- **Dated output** (drill transcripts, measurements, one-off plans)
  goes to `docs/evidence/` and is never edited afterwards.
- **Superseded** docs stay for the decision trail, marked, pointing at
  their successor. Don't execute from them.
- Internal links must resolve; `check_plan_consistency.py` must pass.

## Cutting a release

1. Bump the version **everywhere at once**: `Cargo.toml`,
   `crates/dg-client/Cargo.toml`, root / `client/` / `web/`
   `package.json`, and the lockfiles, in one commit.
2. Move the `CHANGELOG.md` *Unreleased* notes into a dated
   `## [X.Y.Z]` entry.
3. Tag the bump commit: `git tag -a vX.Y.Z`. `dg --version`, the tag,
   and the version stamped into release bundles by
   `scripts/release-bundle.sh` are then the same number.
4. **Pushing the tag and deploying are owner actions.** Deploys and
   rollbacks follow `docs/upgrade-rollback.md`; the deployed host keeps
   running the old version until that procedure runs.

## Lines we don't cross

- Loopback-only bind; no TLS in the gateway by decision — remote access
  rides SSH or the WireGuard mesh (`docs/access-edge.md`).
- Bots and apps get scoped `database_read_only` / `database_read_write`
  keys — never the owner key.
- No live OVH, micro2, or production-data action without the owner's
  separate approval; tests never touch production credentials, servers,
  or buckets.
- The semantics the audit marked sound stay sound: freshness is never
  silently optimistic, writes are idempotent, read-only is enforced by
  SQLite's authorizer, and historical evidence is immutable.

## What only the owner does

- Approve production acceptance and any network-edge change.
- Push release tags; run (or approve) live deploys, restores, and
  drills on micro2.
- Hold and rotate the owner key, bucket credentials, and host access.
