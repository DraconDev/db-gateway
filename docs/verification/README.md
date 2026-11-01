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

### v0.10.0

| Receipt | Revision it tested | Added by |
| --- | --- | --- |
| `full-suite-eeed1a4.*` | `eeed1a4` — the tree that shipped, after the last code change | `6ca6021` |
| `full-suite-c13bf7f.*` | `c13bf7f` — the same tree plus this README | `e63e2f8` |

"Same tree" is meant literally for everything that can break: the last commit
that touched `src/` is `34beb29`, an ancestor of both tested revisions, and no
`src/` change follows it.

The newer receipt records the `dg` artifact it tested (`798d3c5f…`), so it
describes the binary that shipped. The older one does not: its header names
the revision, the branch, the toolchain and the host, but no artifact hash —
that run predates the hash being written into the header. What both receipts
do establish is the tree they tested: the last commit that touched `src/` is
`34beb29`, an ancestor of both, so neither ran against different code.

**No sentence here says which commit the tag currently points at, on purpose.**
Every receipt is added by a docs-only commit on top of what it tested, so each
one moves the tag again; a literal "the tag is X" would be stale the moment it
was written. (Commit ids do appear above, as the immutable "added by" facts.)
Ask git instead:

```sh
git rev-parse v0.10.0^{}                      # the release commit right now
git log -1 --format='%h %s' v0.10.0^{}        # and what it changed
git merge-base --is-ancestor eeed1a4 v0.10.0^{} && echo "tested tree is in the release"
```

Every claim above is checkable with `git log --stat` on the commit that added
the receipt:
`git log --format='%h %s' --diff-filter=A -- docs/verification/full-suite-eeed1a4.log`.

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
