## Summary

<!-- What does this change, and why? Link the issue it resolves. -->

## Checklist

- [ ] `cargo fmt --all --check`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` pass locally
- [ ] Tests cover the change (loopback and mock upstream only, no fixed sleeps)
- [ ] Docs updated where behavior changed
- [ ] `CHANGELOG.md` has an entry under `Unreleased` for user-visible changes
- [ ] No keys, key files or account details are included

## How this was checked against a real provider

<!-- Required for provider changes. Describe what you ran; do not paste credentials. -->
