# Release verification receipts

This directory keeps the **raw output of the release gates** for a tagged
release, next to the release notes rather than in a commit message or a chat
transcript. A gate result that exists only as a summary ("694 passed") cannot
be inspected later, so each release that has one records the actual output.

## What is in here

| File | What it records |
| --- | --- |
| `full-suite-<rev>.log` | The complete, unedited stdout of `cargo test --workspace --all-targets`, with the revision, branch, toolchain, host, clean-worktree check and exit code in the header and footer |
| `full-suite-<rev>-summary.txt` | One line per suite taken from that log, plus the log's SHA-256, so a reader can check the totals without scrolling |

The v0.10.0 receipts are `full-suite-eeed1a4.*` (the code commit the auditor
asked about) and `full-suite-c13bf7f.*` (the commit the release tag points at).
Both record 694 passed, 0 failed across 94 suites, and the release binary hashes
to `798d3c5f…` from either tree, so neither receipt is describing a different
artifact from the one that shipped.

## Why the receipt is a separate, docs-only commit

A log cannot describe the commit that contains it: the run happens first, then
the file is added. So the receipt for revision `R` is committed at `R+1`, a
commit that touches **only this directory**. That makes the claim verifiable
rather than trusting:

```sh
# The only difference between the tested tree and the release tree is
# the receipt itself.
git diff --stat <tested-revision>..<release-commit>
```

If that diff shows anything outside `docs/verification/`, the receipt does not
cover the release. Anyone can re-run the gate themselves; the raw output is
here so the claim can be checked without re-running a 20-minute suite.

## Rules

- Raw output is committed **unedited** — a failing suite is recorded as
  failing. A receipt is evidence, not marketing.
- The header must name the revision that was checked out, so a log can never
  drift to a different tree.
- The worktree must be clean before and after the run, recorded in the log
  itself, so "which tree did this test?" is answered by the file.
- These logs are release receipts, not a substitute for the gate: `cargo test`
  still has to pass on the release commit.
