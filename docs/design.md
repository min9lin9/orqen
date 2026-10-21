# Design principles

Orqen aims to be a gateway that lets a coding agent use several model providers without losing the meaning of tool calls, reasoning state, cancellation or streaming. It starts with personal coding workflows and values consistent agent runs over the number of providers it supports.

## Invariants

1. Only providers that meet the request's tool, streaming and reasoning needs are candidates.
2. Session IDs and reusable reasoning state are kept apart by provider, model and credential scope.
3. Tool call arguments, identifiers, results and their order are preserved.
4. Routing decisions are reproducible: explicit priorities and fixed tie-breaking.
5. Once meaningful output has reached the client, or a cancellation has been observed, Orqen does not call another provider.
6. Quotas, cooldowns, failures and routing records are tracked per real limit scope.

Orqen separates the moment an HTTP response is committed from the moment meaningful text, reasoning or tool arguments are delivered. It does not promise that a provider will stop executing, or stop billing, a request it has already accepted.

## Non-goals

- Running tools remotely on behalf of the client
- Team billing or a hosted multi-user service
- Bypassing provider authentication or terms
- Decrypting or re-signing another provider's encrypted reasoning data

The concrete runtime contract is in [operations.md](operations.md). How Orqen grows to more providers is in [provider-expansion.md](provider-expansion.md).
