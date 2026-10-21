# Changelog

All notable changes to this project are documented in this file. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-21

### Added

- Loopback gateway binary `orqen` with OpenCode Go as the first provider.
- Byte-exact forwarding for `POST /v1/chat/completions`, `POST /v1/messages`, `POST /v1/responses` and `GET /v1/models`, including SSE streams and compressed responses.
- Key pool with account scopes, conversation affinity, and cooldowns for 401, 402 and 429 that honor `Retry-After`.
- Retry on another key with the same model, only before response headers are sent.
- Conversation identity from configurable headers, with `ephemeral` and `reject` policies for requests without one.
- `GET /healthz`, `orqen --check`, graceful shutdown, and a systemd user unit.

[Unreleased]: https://github.com/min9lin9/orqen/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/min9lin9/orqen/releases/tag/v0.1.0
