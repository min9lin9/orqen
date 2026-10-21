# Notes for coding agents

Guidance for AI coding agents working in this repository. Human contributors should start with [CONTRIBUTING.md](CONTRIBUTING.md); everything there applies here too.

## Project

Orqen is a local model-provider gateway for coding agents, written in Rust. It forwards OpenAI- and Anthropic-compatible requests to providers using a pool of API keys. OpenCode Go is the only provider today; [docs/provider-expansion.md](docs/provider-expansion.md) describes how more are added.

## Layout

```text
src/
├── main.rs            CLI, startup, signals, graceful shutdown
├── config.rs          TOML config and key file validation
├── pool.rs            key pool, scoped cooldowns, affinity
├── proxy.rs           routes, forwarding, retries, SSE relay
├── session.rs         conversation identity
├── retry_after.rs     Retry-After parsing and formatting
├── error.rs           gateway error responses
└── integration_tests/ loopback behavior tests with a mock upstream
docs/                  operations, clients, design, provider docs
examples/              annotated config
deploy/systemd/        user unit
```

## Commands

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

## Rules

- Never retry, splice or switch models after response headers reach the client.
- Forward request and response bytes unchanged; read only the top-level `model` field.
- Never log or return key values, key file contents or raw conversation IDs.
- No `unwrap`, `expect` or `unsafe` in production code.
- Tests use loopback and temporary directories only, with event-based waits and bounded timeouts; no fixed sleeps.
- Do not describe mock-upstream test results as tests against a real provider.
- Keep provider-specific behavior in the places listed in [docs/adding-a-provider.md](docs/adding-a-provider.md) until the provider descriptor lands.
