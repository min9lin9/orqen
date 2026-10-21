# Contributing to Orqen

Thanks for your interest. Bug reports, provider requests, documentation fixes and code are all welcome.

## Before you start

- **Bugs:** open a [bug report](https://github.com/min9lin9/orqen/issues/new?template=bug_report.yml). Include your Orqen version, OS, the redacted config, and the log lines around the failure.
- **New providers:** open a [provider request](https://github.com/min9lin9/orqen/issues/new?template=provider_request.yml) before writing code, and read [docs/adding-a-provider.md](docs/adding-a-provider.md).
- **Larger changes:** open a [feature request](https://github.com/min9lin9/orqen/issues/new?template=feature_request.yml) first, so we can agree on the approach before you invest time.
- **Security issues:** follow [SECURITY.md](SECURITY.md). Do not open a public issue.

## Development setup

You need Rust 1.88 or later.

```bash
git clone https://github.com/min9lin9/orqen && cd orqen
cargo build
cargo test
```

Before you push, run the same checks as CI:

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Code guidelines

- No `unwrap`, `expect` or `unsafe` in production code; `Cargo.toml` enforces this with lints.
- Never write key values, key file contents or raw conversation IDs to logs or error messages.
- Keep the forwarding contract in [docs/operations.md](docs/operations.md): bytes pass through unchanged, and nothing is retried after response headers are sent.
- Keep changes focused. A bug fix should not carry an unrelated refactor.

## Tests

- Behavior tests live in `src/integration_tests/`. They run Orqen on a loopback port in front of a scripted mock upstream.
- Use only loopback addresses and temporary directories. Tests must not touch the network or real credentials.
- Wait on events with a bounded timeout. Fixed sleeps and polling loops are not accepted.
- A bug fix comes with a test that fails without the fix.

## Pull requests

1. Fork the repository and branch from `main`.
2. Make your change with tests, and update the docs it affects.
3. Add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md) for user-visible changes.
4. Open the pull request and fill in the template. CI must pass before merge.

Commit messages are short and in the imperative, in English or Korean, for example `Add Kimi provider descriptor`.

## Code of Conduct

By participating, you agree to follow the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

Contributions are licensed under the [MIT License](LICENSE), the same as the project.
