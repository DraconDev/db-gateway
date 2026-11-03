# Publication readiness

Status: **gate shipped; two history decisions are open with the owner**
(see [Open decisions](#open-decisions)). This page is the runbook for
taking this repository public and the record of what the gate found.

The repository was developed private. Private-ness was doing the work of
a control that nothing enforced: any commit could carry a credential, a
transcript, or a deployment hostname, and the only thing standing between
that commit and the internet was a visibility setting nobody re-checked.
`scripts/public-readiness.sh` is that control in executable form.

## Running the gate

```bash
scripts/public-readiness.sh          # tracked tree AND full history
scripts/public-readiness.sh --tree    # tracked tree only (what CI runs)
```

Exit status: `0` ready, `1` a blocker was found, `2` the gate could not
run. Blockers are the things that make publication a leak. Advisories are
disclosed facts that are the owner's call to publish or scrub.

CI runs `--tree` on every push and PR, because the tracked tree is the
thing every future commit changes and therefore the thing a gate can keep
green. The history checks run before publication, not on every PR:
history cannot be un-published, only rewritten, and a rewrite changes
every commit id and needs a force-push to both remotes. That is a
decision for a human, so the gate reports it and stops there.

## What it checks

| Check | Tier | Why it is there |
|---|---|---|
| Session/transcript exhaust in the index | blocker | Verbatim working-session transcripts carry prompts, tool output, file paths, and whatever was on screen. One was tracked (see below). |
| Session/transcript exhaust anywhere in history | blocker | A removed file is still in every clone. |
| Credential files by extension (`.key`, `.pem`, `.age`, `.agekey`, `.env`, keystores) | blocker | A committed key is a published key. |
| Build and runtime output (`target/`, `dist/`, `state/`, `node_modules/`, `.cargo/target*/`) | blocker | A repo carrying its own build output, or a runtime database, is noise at best. |
| Tracked files over 256 KiB | blocker | A blob, not source. The largest legitimate tracked file here is a 184 KiB screenshot. |
| Credential-shaped material in the tracked tree | blocker | Delegated to `scripts/secret-scan.sh`, which CI already runs, so the matcher and its false-positive policy live in exactly one place. |
| Credential-**formed** material in history | blocker | AWS key ids, PEM private-key headers, GitHub tokens. A credential by its form, not just by a variable name. |
| Name-based secret matches in history | advisory | The `generic-secret-assignment` heuristic. Useful per-file, noisy across 2500 commits — see [Why the two tiers](#why-the-history-scan-has-two-tiers). |
| `age`-encrypted secret material in history | blocker | See [Encrypted secrets in history](#encrypted-secrets-in-history). |
| Home directory path, personal email, public IP, cloud account id | advisory | Facts a public reader learns about the operator and the deployment. Not a leak, but not free either. |
| Dirty working tree, unpushed commits | advisory | What you publish should be what was checked. |

## What the gate found

Two of these were real, unfixed leaks in the tracked tree; both are fixed
in this change. The rest are open decisions.

### 1. A tracked session transcript (fixed)

`pi-session-2026-10-22T20-39-53-378Z_*.md` was in the index. It is a
decoded pi session: verbatim prompts, tool calls, tool output. Tracked in
it were this operator's email address, their home directory path, a
public server address, and production database names.

A previous audit (`docs/evidence/audit-2026-09-21.md`, item A1) had
already found and untracked the **`.html`** session export. It swept the
one extension it had seen, so the **`.md`** twin of the same session —
added later by a different export path — walked straight past it.

The fix is not "delete the file". The fix is the ignore rule:
`.gitignore` now ignores `pi-session-*`, `conversation-*`, `rollout-*`,
and `trajectory-*` **below** the managed block, so it wins over the
managed block's blanket un-ignores (`!pi-session-*.html`,
`!conversation-*.md`, `!rollout-*.json`, `!trajectory-*.html`). Those
un-ignores exist because the managed block applies one house style to
every repo on this box; for a repo that is published, that style is a
leak path. Covering every format rather than the extension a previous
audit happened to notice is the point.

### 2. An un-ignored age private key (fixed)

The managed block un-ignores `.dracon/data/` so that the owner **public**
key (`.dracon/data/keys/owner_nixos.pub`) can be tracked — it is tracked
today. Un-ignoring a directory re-includes everything under it, so an
`age` private key (`.age`) or a plaintext age identity (`.agekey`)
dropped in `.dracon/data/keys/` was **committable**. Nothing in the tree
was a private key; the hole was one `cp` wide.

`.gitignore` now ignores `*.age` and `*.agekey` below the managed block.
The tracked public key still tracks.

### 3. `note.md` (owner's call, reported)

`note.md` is a three-line personal note ("because this is us we have cool
options such as we can read queries like to check which one was heavy").
It is not a leak and it is not doing any harm, but it is a thought
addressed to the operator sitting in the root of a project that would be
someone's first impression. It stays until the owner says otherwise;
this page is the register that it was noticed and left deliberately.

## Open decisions

These need the owner, not an agent. The gate reports them; it does not
perform them.

### Encrypted secrets in history

`dracon-warden` encrypts secret-ish values in place and leaves them in
the repo as `[DRACON_SECRET:<age-ciphertext>]`. Two different things got
that treatment:

- **Rehearsal/sandbox credentials.** In `scripts/s3-rehearsal-endpoint.sh`
  the encrypted default belongs to `DG_REHEARSAL_KEY_ID`, and its sibling
  `DG_REHEARSAL_SECRET` defaults to the literal string
  `rehearsal-secret-not-a-real-credential`. That is sandbox material.
- **Test fixtures.** `tests/secret_scan.rs` and
  `tests/operations_scripts.rs` hold credential-shaped strings **on
  purpose** — they exist to prove the scanner reports real shapes and
  ignores fake ones. Several of those fixtures were encrypted too.

Not found: production credentials. Nothing in the encrypted set is named
for production, and the plaintext fixtures around them are keyboard-mash.

The decision is still the owner's, because the ciphertext is not free:
publishing it puts the payload and the age recipient in every clone
forever, and the owner's private key is then the only thing between the
internet and those values. A key that leaks in 2029 decrypts a commit
published in 2026. Encrypting a secret does not make publishing it a
non-event; it makes it a deferred event.

Options: rewrite them out with `git filter-repo` (changes every commit
id, force-push to both remotes), or publish them and accept that the age
identity is the whole of the protection. There is no third option where
the gate can decide for you.

### Session transcripts in history

Two session paths were committed:

- `pi-session-2026-09-17T...html` — inspected during this work: it
  contains no email, no private key, and no public IP. Benign.
- `pi-session-2026-10-22T...md` — the file removed from the index above.

The `.html` one is already deleted from the tree but still in history.
Removing the `.md` from the index does not remove it from history either.
Same options as above, and the same reason the gate will not do it
unattended.

### Identity disclosure

Reported as advisories, not blockers. `scripts/public-readiness.sh` splits
them into **live docs** (editable — scrubbing is a choice you can make
now) and **frozen records** (`docs/evidence/`, `docs/verification/` —
never edited after the fact per `docs/README.md`, so publishing them is
the decision):

- `/home/dracon` paths in 5 live docs and 10 frozen records.
- The deployment host's public IP in 2 live docs and 4 frozen records.
- A cloud account id in 1 live doc.

The cloud account id is in `docs/audit-2026-10-19.md`, in the A1
credential-rotation section, which also records that the AWS key was left
unrotated by owner decision (2026-10-19) and that its IAM user lives in
that account. That reasoning is sound for a private repo. Publishing the
page publishes the account id and the fact that a working key for it
exists, so either scrub the account id from that item or accept that it
is public.

## Why the history scan has two tiers

The scanner's `generic-secret-assignment` pattern is a heuristic: a
secret-ish *name* assigned a long value. On one file it earns its keep.
Across 2500 commits it also matches

- constants whose **value** is an environment-variable name rather than a
  secret (`SECRET_PREFIX = "LITESTREAM_S3_SECRET_ACCESS_KEY="`), and
- the fixtures in `tests/secret_scan.rs` and
  `tests/operations_scripts.rs`, which contain credential shapes because
  testing a credential scanner requires them.

The gate splits these from the patterns that match a credential *shape*
(AWS key ids, PEM headers, GitHub tokens). Shaped hits block; name-based
hits advise. `scripts/secret-scan.sh` itself is unchanged: it is a tested
security control, and loosening a detector so that a run comes back green
is how a scanner becomes decoration.

A shaped hit can still be a fixture — `tests/secret_scan.rs` is the
scanner's own fixture suite, so a hit there is expected. Read the hit
before treating it as a leak. The gate cannot tell a fixture from a
credential, and a gate that claimed to would be lying.

## Before the visibility button

1. `scripts/public-readiness.sh` — every blocker cleared or explicitly
   recorded as a decision above.
2. The history decisions in [Open decisions](#open-decisions) settled.
   If they were settled by a rewrite, re-run the full gate afterwards;
   the rewrite is what makes the history checks pass.
3. A push, so `main` and the remote agree (`git status` clean, nothing
   ahead of upstream).
4. Then flip visibility.

Rotating a credential that was ever real is not optional and not
conditional on the rewrite: scrubbing history removes it from the diff,
not from anyone who already cloned.
