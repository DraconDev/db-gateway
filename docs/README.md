# Documentation index

This tree accumulated 70+ files in five weeks: contracts and runbooks
mixed with dated drill transcripts, one-off plans, and audits of audits.
This index is the register that resolves the competition. Every file is
one of:

- **Canonical** — the current statement of its subject. Edit in place;
  when one of these changes meaning, the others must be brought along
  (`scripts/check_plan_consistency.py` enforces the plan register below).
- **Historical evidence** — a dated record of something that was run or
  measured. Never edited after the fact; lives under `evidence/`. Cite
  it as provenance, never as current instruction.
- **Superseded** — a plan, proposal, or audit whose successor exists.
  Kept for the decision trail; do not execute from it.

**Start here:** [`personal-turso-megaplan.md`](personal-turso-megaplan.md)
is the canonical roadmap and source-of-truth register (the root
`TASKS.md` is its live checklist; the root `README.md` is the project
front door). For operations, the living contracts and runbooks are the
canonical list below. For "what did we actually measure", go to
`evidence/` via the groups at the bottom.

## Canonical

| File | What it is |
|---|---|
| `personal-turso-megaplan.md` | The canonical roadmap + source-of-truth register. CI-checked. |
| `audit-2026-10-19.md` | The current audit register (the A-series); living until the next audit supersedes it. |
| `architecture.md` | Target architecture and decision register (register member). |
| `api.md` | HTTP/CLI API contract: routes, value encoding, errors, retry semantics (register member). |
| `access-edge.md` | Access/edge design: mesh/app proxies, socket activation, loopback rules. |
| `backup-compatibility.md` | Backup-format compatibility assessment against public implementations. |
| `cli-turso-mapping.md` | How the `dg` CLI maps to the Turso/libSQL surface it replaces. |
| `tui.md` | The terminal operator view (`dg tui`): the tabs, the keys, the actions, and the rules it holds. |
| `deployment.md` | The deploy guide. `scripts/validate-deploy.sh` checks units against it. |
| `deployment-readiness.md` | micro2 deployment acceptance checklist. |
| `later-capabilities.md` | Deferred-capability decisions: what was postponed and why. |
| `micro2-measurement-runbook.md` | How host measurements (soaks, sampling) are run on micro2. |
| `migration-cutover.md` | Migration cutover plan (register member). |
| `migration-inventory.md` | Migration source/target inventory and per-item status. |
| `observability-contract.md` | The observability contract: signals, alerts, the 3 a.m. rules (register member). |
| `ovh-sandbox-runbook.md` | Runbook for the sandbox S3 rehearsal packet. |
| `recovery-contract.md` | Recovery contract: restore guarantees and drills (register member). |
| `recovery-decisions.md` | Recovery decision record (register member). |
| `recovery-policy.md` | Recovery policy the deploy units and drill scripts implement. |
| `resource-budget.md` | The measured 1 GiB host budget; `validate-deploy.sh` checks MemoryMax against it. |
| `resource-harness.md` | The resource-measurement harness: what the numbers mean. |
| `upgrade-rollback.md` | Upgrade and rollback procedure for the deployed gateway. |

## Superseded

| File | Superseded by / status |
|---|---|
| `plan-2026-09-22.md` | The megaplan. Self-marked historical. Stays at this path: the plan register names it (`REQUIRED_REGISTER_PATHS` in `check_plan_consistency.py`). |
| `scale-proposal-2026-09-21.md` | Phases 1–2 implemented; direction absorbed into the megaplan. Stays at this path: register member. |
| `evidence/audit-2026-09-21.md` | `audit-2026-10-19.md`. The first full audit; its findings are closed or carried forward. |
| `evidence/audit-2026-09-26-cli-console.md` | `audit-2026-10-19.md`. Point-in-time CLI/console audit. |
| `evidence/owner-decisions-2026-09-21.md` | Never accepted (proposed memos). Kept as the decision trail. |
| `evidence/pilot-plan-2026-10-16.md` | The pilot it planned has run; see the pilot records under evidence. |

## Historical evidence (`evidence/`)

Dated artifacts, grouped by campaign. New drill/measurement output goes
here, named `<subject>-<YYYY-MM-DD>.<log|json>`. One register-named
exception stays at the root: `readiness-summary.md` — a dated
2026-09-20 snapshot that banners itself superseded (current status
lives in `TASKS.md`, `deployment-readiness.md`, and
`audit-2026-10-19.md`) and is classed historical evidence by the
megaplan register, which names its path; it does not move.

- **First deploy & micro2 sessions** — `first-deploy-micro2.md`,
  `first-deploy-micro2-evidence.log`, `micro2-2026-09-21.md`,
  `micro2-2026-09-22-evidence.log`, `host-audit-micro2-2026-10-14.json`,
  `fresh-host-bucket-drill-2026-10-17.log`, `watchdog-2026-10-17.log`.
- **Restore & recovery drills** — `m3-restore-2026-10-15.md`,
  `m3-restore-drill-2026-10-15.log`,
  `registry-recovery-drill-2026-10-19.log`,
  `recovery-rehearsal-evidence.json`,
  `litestream-replication-evidence.log`,
  `litestream-sync-drill-2026-10-19.log`,
  `migration-cdev-trial-2026-10-19.log`,
  `wipe-restore-2026-09-21-evidence.log`, `wipe-restore-evidence.log`.
- **Sandbox packet (OVH S3 gates)** — `sandbox-packet-2026-09-28-rehearsal.log`,
  `sandbox-packet-2026-09-29-rehearsal.log`,
  `sandbox-packet-2026-09-29-rehearsal.matrix.json`,
  `sandbox-packet-2026-10-13-ovh.log`, `sandbox-packet-2026-10-14-ovh.log`,
  `first-resource-report-release-2026-10-10.json`.
- **Pilot (billing_dev_pilot)** — `pilot-day0-2026-10-16.md`,
  `pilot-soak-log.md`, `pilot-rollback-rehearsal-2026-10-16.log`.
- **Load, soak & resource evidence** — `benchmark-2026-09-21.md`,
  `comparison-2026-09-21.md`, `e2e-test-campaign-2026-10-15.log`,
  `m4-failure-visibility-2026-10-15.json`, `m4-failure-visibility-2026-10-15.log`,
  `m4-micro2-headroom-2026-10-15.md`, `m4-micro2-sampling-2026-10-16.log`,
  `noisy-neighbor-cross-db-2026-10-11.json`, `noisy-neighbor-cross-db-2026-10-11.log`,
  `resource-report-release-1G-scope-2026-10-11.json`,
  `resource-report-release-1G-scope-2026-10-15.json`,
  `soak-2026-09-21-evidence.log`, `soak-2h-evidence.log`,
  `soak-30min-evidence.log`, `soak-starvation-2026-10-11.log`,
  `soak-starvation-evidence-2026-10-11.json`, `soak-starvation-evidence.log`,
  `upgrade-rehearsal-2026-09-21-evidence.log`, `upgrade-rehearsal-evidence.log`.
- **Host rehearsals & scanner reviews** — `clean-host-rehearsal-2026-10-19.log`,
  `clean-host-rehearsal-2026-10-19T232000Z.log`,
  `sync-fixture-review-2026-10-20.md`.
- **CLI wait-column evidence** — `cli-wait-columns-2026-10-21.md`
  (with the rendered capture `dg-status-wait-columns-2026-10-21.png`).
- **CLI pet-trio campaigns** — `pet-livefire-2026-10-17.log`,
  `pet-trio-2026-10-16.log`, `pet-trio-extensive-2026-10-16.log`,
  `pet-trio-suites-2026-10-16.log`.

## Rules

1. A new dated artifact (drill transcript, measurement, campaign log)
   goes in `evidence/`, never beside the contracts.
2. A canonical doc is edited in place; if it stops being true, it is
   demoted here (marked superseded with its successor named), not
   silently left to compete.
3. The plan register (`personal-turso-megaplan.md` +
   `check_plan_consistency.py`) is changed deliberately and in one
   place; register members do not move.
