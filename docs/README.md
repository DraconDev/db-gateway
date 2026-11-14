# Documentation index

This tree holds the current contracts and runbooks for the gateway,
alongside superseded plans kept for the decision trail. Every file is
one of:

- **Canonical** — the current statement of its subject. Edit in place;
  when one of these changes meaning, the others must be brought along
  (`scripts/check_plan_consistency.py` enforces the plan register below).
- **Superseded** — a plan, proposal, or audit whose successor exists.
  Kept for the decision trail; do not execute from it.

**Start here:** [`../README.md`](../README.md) is the project front door
and [`deployment.md`](deployment.md) is the generic deploy procedure.
This gateway is in pre-production development; it is not ready for production use or production data.

## Canonical

| File | What it is |
|---|---|
| `personal-turso-megaplan.md` | Generic project overview (pre-production). |
| `audit-2026-10-19.md` | The current audit register (the A-series); living until the next audit supersedes it. |
| `architecture.md` | Target architecture and decision register (register member). |
| `api.md` | HTTP/CLI API contract: routes, value encoding, errors, retry semantics (register member). |
| `access-edge.md` | Access/edge design: mesh/app proxies, socket activation, local rules. |
| `backup-compatibility.md` | Backup-format compatibility assessment against public implementations. |
| `cli-turso-mapping.md` | Generic CLI setup note. |
| `turso-surface-gap.md` | Generic surface note (pre-production). |
| `tui.md` | The terminal operator view (`dg tui`): the tabs, the keys, the actions, and the rules it holds. |
| `deployment.md` | The deploy guide. `scripts/validate-deploy.sh` checks units against it. |
| `deployment-readiness.md` | deployment-host deployment acceptance checklist. |
| `later-capabilities.md` | Generic capabilities note. |
| `measurement-runbook.md` | How host measurements (verifications, sampling) are run on deployment-host. |
| `migration-cutover.md` | Generic migration note (no production migration). |
| `migration-inventory.md` | Generic migration note (no production migration). |
| `no-turso-contact-2026-10-31.txt` | The trace behind the comparison's Turso-contact disclosure: commands by date, their reported results, and the limits of what the credential check establishes. |
| `observability-contract.md` | The observability contract: signals, alerts, the 3 a.m. rules (register member). |
| `shadow.md` | Shadow mode: mirroring a live application's writes to dg and comparing the copies, before any move. |
| `sandbox-runbook.md` | Runbook for the sandbox rehearsal packet. |
| `publication-readiness.md` | What it takes to take this repository public: `scripts/public-readiness.sh`, what the gate checks, the leaks it found and fixed, and the history decisions that are the owner's. |
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
| `readiness-summary.md` | A dated 2026-09-20 snapshot that banners itself superseded (current status lives in `TASKS.md`, `deployment-readiness.md`, and `audit-2026-10-19.md`). |

## Rules

1. A canonical doc is edited in place; if it stops being true, it is
   demoted here (marked superseded with its successor named), not
   silently left to compete.
2. The plan register (`check_plan_consistency.py`) is changed deliberately and in one
   place; register members do not move.
