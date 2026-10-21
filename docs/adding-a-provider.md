# Adding a provider

Orqen has one built-in provider today, OpenCode Go. Phase 1 of the [roadmap](../ROADMAP.md) gathers its specifics into a single provider descriptor; after that, a provider will be one new file plus tests. Until then, this page lists where provider-specific behavior lives, so a contributor can see the size of the change. The design behind it is in [provider-expansion.md](provider-expansion.md).

## 1. Open a provider request first

Use the [provider request template](https://github.com/min9lin9/orqen/issues/new?template=provider_request.yml). It asks the questions that decide whether a provider fits:

- How does it authenticate? Orqen supports documented API keys only.
- Which protocol does it speak: OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages?
- What does it return when a key, an account or a model hits a limit? Does it send `Retry-After`?
- Does it use a header for conversation affinity or caching?
- Do its terms allow traffic through a self-hosted proxy?

## 2. Where OpenCode Go specifics live

| Concern | Location |
| --- | --- |
| Default upstream URL | `DEFAULT_UPSTREAM` in `src/config.rs` |
| Local route to upstream path | `Route::upstream_path` in `src/proxy.rs` |
| Credential header for each route | `upstream_request` in `src/proxy.rs`, and `Credential::bearer` / `Credential::api_key` in `src/pool.rs` |
| Conversation header sent upstream | `forward_header` in `src/session.rs` |
| Client headers passed through | `forwardable` in `src/proxy.rs` |
| Error body shape for each route | `Route::shape` in `src/proxy.rs`, and `src/error.rs` |
| Mapping 401, 402 and 429 to cooldowns | `rejection_outcome` in `src/proxy.rs`, and `Pool::report` in `src/pool.rs` |

A provider that mirrors OpenCode Go's paths, credential headers and limit responses exactly needs no code: point `upstream.base_url` at it and run a separate Orqen instance. Anything else touches the places above. Until phase 1 lands, coordinate in the issue before writing code, so the change can follow the descriptor shape.

## 3. Tests

Behavior tests live in `src/integration_tests/`. Each test starts Orqen on a loopback port in front of a scripted mock upstream and checks what reached the upstream and what came back to the client. A provider change needs tests for:

- route mapping and the upstream path
- the credential header for each route, and that client credentials are dropped
- the conversation header forwarded upstream
- each limit response and the cooldown scope it produces
- byte-exact request and SSE forwarding

Tests use only loopback addresses and temporary directories, never real credentials or the network. Wait on events with a bounded timeout; do not use fixed sleeps or polling.

## 4. Documentation

In the same pull request:

- add the provider to the provider table in [README.md](../README.md) and [README.ko.md](../README.ko.md)
- document its routes in [operations.md](operations.md#routes)
- add an entry under `Unreleased` in [CHANGELOG.md](../CHANGELOG.md)

Describe in the pull request how you checked the provider against a real account. Keep credentials and account details out of the repository.
