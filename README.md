# Orqen

[![CI](https://github.com/min9lin9/orqen/actions/workflows/ci.yml/badge.svg)](https://github.com/min9lin9/orqen/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A local model-provider gateway for coding agents.** [한국어](README.ko.md)

Orqen runs on your machine between a coding agent and model providers. Point any OpenAI- or Anthropic-compatible client at it, such as omo or Claude Code, and Orqen spreads requests across your API keys, keeps each conversation on one key, and handles rate limits before a response starts. It does not rewrite request or response bytes.

Orqen is early (v0.1). It supports one provider today, [OpenCode Go](https://opencode.ai), and more are planned. See the [roadmap](ROADMAP.md).

## Why

- **Several keys, one endpoint.** Register keys from one or more accounts; your agent sees a single local URL.
- **Conversation affinity.** A conversation keeps landing on the same key, which helps provider-side caching.
- **Rate limits handled by scope.** A 429 cools down the whole account or only that model, as you configure. Orqen retries on another eligible key, and only before any byte reaches the client.
- **Nothing rewritten.** Tool calls, reasoning blocks and SSE streams pass through unchanged.

## Features

### Providers

| Provider | Status |
| --- | --- |
| OpenCode Go | Supported |
| OpenCode Zen, OpenRouter, z.ai, Kimi, MiniMax, Alibaba Coding Plan | Candidates, see [provider expansion](docs/provider-expansion.md#candidate-providers) |

### Client API

| Route | Protocol |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI Chat Completions, streaming and tool calls included |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | Model list |
| `GET /healthz` | Health, and cooldown state for each key |

### Key pool

- Each key lives in its own file and belongs to an account scope.
- 401 blocks that credential, 402 blocks the account, and 429 blocks the account or one model. `Retry-After` is honored.
- Before response headers are sent, a 401, 402 or 429 is retried on another eligible key with the same model.
- 5xx responses pass through unchanged; retrying them is left to the client.

### Conversation identity

- Orqen identifies a conversation from `x-opencode-session`, `x-omo-session`, `x-session-id` or `session_id`; the list is configurable.
- Requests without an identity header get a one-off session (`ephemeral`) or are rejected (`reject`).

### Operations

- One TOML file; secrets stay in separate key files.
- `orqen --check` validates the config and key files before you start.
- Graceful shutdown drains in-flight responses. A systemd user unit is included.
- Logs carry public key IDs and conversation hashes, never secrets or raw IDs.

## Quick start

You need Rust 1.88 or later and an OpenCode Go API key.

```bash
git clone https://github.com/min9lin9/orqen && cd orqen
cargo build --release

mkdir -p ~/.config/orqen/keys
printf '%s\n' "$OPENCODE_GO_API_KEY" > ~/.config/orqen/keys/go-1.key
chmod 600 ~/.config/orqen/keys/go-1.key
cat > ~/.config/orqen/orqen.toml <<'EOF'
[[keys]]
id = "go-1"
key_file = "keys/go-1.key"
EOF

./target/release/orqen --check
./target/release/orqen
```

Orqen listens on `127.0.0.1:8790`. In another terminal, check that it is up and can reach the provider:

```bash
curl -s http://127.0.0.1:8790/healthz
curl -s http://127.0.0.1:8790/v1/models
```

Every setting, with comments, is in [`examples/orqen.example.toml`](examples/orqen.example.toml).

## Connect a client

| Client protocol | Base URL |
| --- | --- |
| OpenAI-compatible | `http://127.0.0.1:8790/v1` |
| Anthropic-compatible | `http://127.0.0.1:8790` |

Clients may send any placeholder API key; Orqen replaces it with a pooled key. Setup for omo, Claude Code, the OpenAI SDK and curl is in [docs/clients.md](docs/clients.md).

## Documentation

- [Connecting clients](docs/clients.md)
- [Operations guide](docs/operations.md): configuration, routes, key pool, retries, limits, shutdown, logging
- [Design principles](docs/design.md)
- [Provider expansion design](docs/provider-expansion.md)
- [Adding a provider](docs/adding-a-provider.md)
- [Roadmap](ROADMAP.md)
- [Changelog](CHANGELOG.md)

## Known limitations

- One provider (OpenCode Go).
- No protocol translation: a client must speak a protocol the provider serves.
- `POST /v1/messages/count_tokens` is not implemented.
- Cooldown state is in memory and resets on restart.

## Contributing

Bug reports, provider requests and pull requests are welcome. Start with [CONTRIBUTING.md](CONTRIBUTING.md). This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). Report security issues as described in [SECURITY.md](SECURITY.md), not in public issues.

## Acknowledgements

The provider expansion plan draws on the provider catalog and descriptor design of [CodexBar](https://github.com/steipete/CodexBar).

## License

[MIT](LICENSE)
