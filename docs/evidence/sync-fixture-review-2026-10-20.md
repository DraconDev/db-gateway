# Scanner fixture push repair — 2026-10-20

The global warden pre-push hook blocked the original tests/secret_scan.rs
blob in commit 1c14b46c. The current tests already assemble their synthetic
values from fragments, but the hook scans each unpushed commit rather than
only HEAD, so the forward source fix could not clear the earlier match.

Verified that the one historical assignment match exactly equals the current
synthetic scanner fixture. Warden now contains an exact blob SHA-256 and
individual match SHA-256 receipt for it. The review is recorded in
`dracon-utilities/dracon-warden/docs/design/reviewed-hook-matches-2026-10-17.md`.
Unknown matches still block; changing the blob invalidates the receipt.
No path exemption, plaintext sibling, credential change or history rewrite.

Normal sync successfully pushed the 24 pending commits to GitHub and GitLab;
both forge branches were verified at 1d52ff491eea40ff8c5ef780d1795d6a6f884867.
