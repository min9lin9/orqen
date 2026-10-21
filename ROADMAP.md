# Roadmap

Orqen is early. This roadmap shows the order of work; it has no dates. Details for the provider work are in [docs/provider-expansion.md](docs/provider-expansion.md).

## Now: v0.1

- OpenCode Go provider with byte-exact forwarding of Chat Completions, Messages, Responses and model listing
- Key pool with per-account and per-model cooldowns and conversation affinity
- Retry on another key for 401, 402 and 429, only before response headers are sent

## Next

1. **Provider descriptor.** Move the OpenCode Go specifics into a single descriptor with no behavior change. This makes every later provider a small, reviewable change.
2. **Multiple providers.** `[[providers]]` in the config, per-provider key pools, and model-prefix routing (`provider/model`).
3. **First new providers.** Chosen from the candidate list in [docs/provider-expansion.md](docs/provider-expansion.md#candidate-providers), starting with those that already speak OpenAI- or Anthropic-compatible protocols with API keys.
4. **Client guides.** Tested setup guides for more coding agents.

## Later

- **Quota-aware key selection.** Use documented provider usage APIs to prefer keys with headroom before a request fails.
- **Opt-in failover between equivalent models.** Only before response headers, and only between models the user declares equivalent.
- **Release binaries** for Linux and macOS built in CI on each tag.

## Not planned

- Protocol translation between OpenAI and Anthropic formats
- Credentials taken from browser sessions, desktop apps or consumer OAuth logins
- Retrying or splicing a response after it has started streaming

Suggestions are welcome. Open a [feature request](https://github.com/min9lin9/orqen/issues/new?template=feature_request.yml) or a [provider request](https://github.com/min9lin9/orqen/issues/new?template=provider_request.yml).
