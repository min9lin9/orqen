# Provider expansion design

**Status:** proposal. Orqen ships one provider today, OpenCode Go. This document describes how it grows to many providers, and which ideas it takes from [CodexBar](https://github.com/steipete/CodexBar). The phases are tracked in [ROADMAP.md](../ROADMAP.md); the practical steps for contributors are in [adding-a-provider.md](adding-a-provider.md).

## Starting point: CodexBar

CodexBar is an MIT-licensed menu bar and desktop app for macOS and Linux. It tracks remaining usage on AI coding plans and registers more than 100 provider IDs. Its provider docs ([provider.md](https://github.com/steipete/CodexBar/blob/main/docs/provider.md), [providers.md](https://github.com/steipete/CodexBar/blob/main/docs/providers.md)) are the most complete public map of coding-plan providers we know of.

CodexBar solves a different problem than Orqen. It reads quota; it does not forward inference requests, translate protocols, or pool keys. Most of its fetch strategies read browser cookies, local app storage, or a signed-in CLI session.

### What Orqen takes from it

1. **A descriptor-driven provider registry.** In CodexBar, adding a provider means one folder, one descriptor with its fetch strategies, then tests and docs. The descriptor is the single source of truth for labels, URLs, defaults and the fetch pipeline, and a flat manifest lists every provider. Orqen adopts the same shape for inference: one `ProviderDescriptor` per provider, listed in one place.
2. **Quota awareness.** CodexBar documents each provider's usage endpoint and quota shape, such as five-hour and weekly windows. Orqen currently learns about limits only after the fact, from 401, 402 and 429. A usage probe per provider would let the pool skip a key before a request fails on it.
3. **A catalog of candidates.** CodexBar's provider list shows which coding plans exist and how they authenticate. Orqen's candidate list below starts from it.
4. **Separate surfaces stay separate.** CodexBar treats OpenCode and OpenCode Go as two providers because their auth and quota shapes differ. Orqen follows the same rule.

### What Orqen does not take

- **Code.** CodexBar is a Swift UI application, and none of it maps onto a Rust HTTP gateway. If code is ever ported, its MIT notice goes into [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md).
- **Cookie, local-storage and CLI-session credentials.** A gateway authenticates with documented API credentials only.
- **Personal subscription logins.** Consumer plans that authenticate through an interactive OAuth login are meant for one person's own client. Relaying them through a gateway may breach the provider's terms.

## Target architecture

### Provider descriptor

Each provider is described by one value:

| Field | Purpose | OpenCode Go today |
| --- | --- | --- |
| `id` | stable identifier used in config | `opencode-go` |
| `base_url` | default upstream | `https://opencode.ai/inference/go` |
| `routes` | upstream path for each local route | `/openai/v1/chat/completions`, `/anthropic/v1/messages`, `/openai/v1/responses`, `/v1/models` |
| `auth` | credential header for each route | `Authorization: Bearer`, or `x-api-key` for Messages |
| `session_header` | header the provider uses for conversation affinity, if any | `x-opencode-session` |
| `limits` | how 401, 402 and 429 map to credential, account and model cooldowns | as in [operations.md](operations.md#key-pool-and-cooldowns) |
| `usage_probe` | optional endpoint and quota windows | none yet |

### Configuration

```toml
[[providers]]
id = "opencode-go"
# base_url = "..."   # optional override of the descriptor default

[[keys]]
id = "go-1"
provider = "opencode-go"
key_file = "keys/go-1.key"
scope = "account-1"
```

A config without `[[providers]]` keeps its current meaning: a single OpenCode Go provider. Existing files keep working.

### Routing between providers

With more than one provider, Orqen has to decide where each request goes. The proposal is a model namespace: a request for `opencode-go/deepseek-v4.1-flash` goes to the `opencode-go` provider, and the prefix is stripped before forwarding. An unprefixed model goes to the default provider. `GET /v1/models` merges every provider's list and adds the prefixes.

### Invariants that do not change

- A request is served by the model it asked for. Orqen never switches models or providers silently.
- Nothing is retried after response headers reach the client.
- Request and response bytes are forwarded unchanged.
- Cooldowns follow real account scopes, which now also belong to a provider.

## Phases

1. **Descriptor refactor.** Move the OpenCode Go specifics listed in [adding-a-provider.md](adding-a-provider.md) into a descriptor, with no behavior change; the existing test suite has to pass unchanged.
2. **Multiple providers.** Add `[[providers]]`, per-provider key pools, and model-prefix routing.
3. **Quota-aware selection.** Add usage probes for providers that document a usage API, and prefer keys with headroom.
4. **Opt-in failover.** Before response headers only, fail over between models the user explicitly declares equivalent, across providers. This stays off by default.

Protocol translation, for example serving Anthropic Messages from an OpenAI-only provider, is not scheduled.

## Candidate providers

A provider becomes a candidate when it meets all of these:

1. Documented API-key authentication intended for programmatic use.
2. An endpoint compatible with OpenAI Chat Completions or Responses, or Anthropic Messages, so Orqen can forward it without translation.
3. Terms that allow your own traffic to pass through a self-hosted proxy.
4. Rate-limit responses Orqen can classify, ideally 429 or 402 with `Retry-After`.

The providers below appear in CodexBar's catalog with API-key access, or are aggregators that use API keys. Each still needs a check against the criteria before work starts.

| Provider | Note |
| --- | --- |
| OpenCode Zen | Same vendor as OpenCode Go; a separate surface with its own billing |
| OpenRouter | OpenAI-compatible aggregator |
| z.ai (GLM Coding Plan) | API token access |
| Kimi (Moonshot) | Kimi Code API key access |
| MiniMax | Coding Plan API token access |
| Alibaba Coding Plan | API key access |

Out of scope: plans that authenticate only through browser sessions, desktop apps or consumer OAuth logins.

To propose a provider, open a [provider request](https://github.com/min9lin9/orqen/issues/new?template=provider_request.yml).
