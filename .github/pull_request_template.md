## Summary

<!-- What changed and why. Link the PR id from PLAN.md (e.g. "PR 0.2"). -->

## Acceptance checklist

<!-- Copy the PR's *Accept* line from PLAN.md and tick each item with the actual result. -->

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] PR-specific acceptance check(s): <!-- result -->
- [ ] UI changes: screenshot attached (or N/A)

## Not verified / known gaps

<!-- Be honest. "None" is fine when true. -->

## Housekeeping

- [ ] Out-of-scope ideas went to `docs/BACKLOG.md`, not into this PR
- [ ] No GPUI / GPUI Kit use outside `crates/app`
- [ ] `gpui-kit` not bumped (version bumps are their own PR)
