# Operations guide

This guide covers how Orqen behaves at runtime: configuration, routes, conversation identity, the key pool, retries, limits, shutdown, and logging. For connecting clients, see [clients.md](clients.md).

## Configuration

Orqen reads one TOML file. It looks in this order:

1. `--config PATH`
2. `$ORQEN_CONFIG`
3. `$XDG_CONFIG_HOME/orqen/orqen.toml`
4. `~/.config/orqen/orqen.toml`

`orqen --check --config PATH` validates the file and the key files it references, then exits. The full annotated example is [`examples/orqen.example.toml`](../examples/orqen.example.toml).

- The config file holds no secrets. Each `[[keys]]` entry points at a `key_file` that contains exactly one provider API key. Use `chmod 600`; Orqen warns when group or other users can read a key file.
- Relative `key_file` paths resolve against the config file's directory.
- Key files are read once at startup. Restart Orqen after rotating a key.
- `id` and `scope` are public labels. They appear in logs, `/healthz`, and the `x-orqen-key` response header. Give keys that belong to the same account the same `scope`. A key without `scope` is treated as its own account.
- Startup fails if the same secret appears in two entries, so one account cannot pose as two independent quotas.
- Startup fails if there is no `[[keys]]` entry.
- `listen` must be a loopback address. Non-loopback addresses are rejected at startup.

## Routes

| Local route | Upstream (`upstream.base_url` + path) | Credential header |
| --- | --- | --- |
| `POST /v1/chat/completions` | `/openai/v1/chat/completions` | `Authorization: Bearer` |
| `POST /v1/messages` | `/anthropic/v1/messages` | `x-api-key` |
| `POST /v1/responses` | `/openai/v1/responses` | `Authorization: Bearer` |
| `GET /v1/models` | `/v1/models` | `Authorization: Bearer` |
| `GET /healthz` | local | none |

The upstream paths above are the OpenCode Go layout, the only provider built in today. See [adding-a-provider.md](adding-a-provider.md) for where this mapping lives.

## Forwarding

- Request bodies are forwarded byte for byte. Orqen reads only the top-level `model` string, and passes the model name through unchanged.
- Responses are relayed chunk by chunk without parsing or re-encoding, so SSE bytes and ordering, tool call IDs, arguments and results, and reasoning blocks reach the client as the provider sent them.
- From the client request, Orqen forwards only `content-type`, `accept`, `anthropic-version`, `anthropic-beta`, `openai-beta`, `user-agent`, and `x-opencode-*`. It drops the client's `Authorization` and `x-api-key` and substitutes a pooled credential.
- Orqen never decompresses. Automatic gzip, brotli, deflate and zstd decoding is off, `Content-Encoding` and the encoded bytes pass through, and only `Content-Length` and hop-by-hop headers are removed. Orqen does not send its own `Accept-Encoding`.
- Redirects are not followed. A loopback upstream bypasses any HTTP proxy from the environment.
- SSE is never buffered whole. Client write backpressure sets the pace of upstream reads.

## Conversation identity

- Orqen takes the first present header listed in `session.headers` as the conversation ID. The default order is `x-opencode-session`, `x-omo-session`, `x-session-id`, `session_id`.
- The ID drives key affinity and is forwarded upstream as a per-conversation `x-opencode-session`. There is no global session ID.
- A header that repeats, contains whitespace or non-ASCII bytes, or exceeds 256 characters is rejected with 400. A whitespace-only value counts as absent.
- When no identity header is present, `session.missing` decides:
  - `ephemeral` (default) mints a fresh random ID (`orqen-ephemeral-…`) per request. Separate conversations never merge into one upstream session, but such requests get no affinity and no upstream cache continuity. `/healthz` reports the count as `ephemeral_sessions`.
  - `reject` answers 400 before calling the provider.
- Logs carry a short hash of the conversation ID and the name of the header it came from, never the ID itself.

## Key pool and cooldowns

| Upstream response | Blocked scope | Duration |
| --- | --- | --- |
| `401` | that credential only | `auth_failure_cooldown_secs` |
| `402` | the whole account scope, all models | `Retry-After`, else `default_cooldown_secs` |
| `429` with `rate_limit_scope = "account"` | the whole account scope, all models | `Retry-After`, else `default_cooldown_secs` |
| `429` with `rate_limit_scope = "model"` | (account scope, requested model) | as above |

- Keys in one scope share cooldowns. After a 429, Orqen does not retry on another key from the same account.
- `Retry-After` is parsed as integer seconds or an HTTP date; `retry-after-ms` wins when present. Every value is capped at `max_cooldown_secs`.
- Key choice uses rendezvous hashing over (conversation ID, scope) and (conversation ID, key ID). A conversation stays on the same account and key while they remain eligible, and Orqen keeps no per-conversation state.
- The model cooldown table holds at most `max_model_cooldowns` entries and never drops an active cooldown. When the table is full, expired entries are pruned first. If it is still full of active entries, the new model limit is applied to the whole account scope until its deadline and a warning is logged. This is deliberately conservative: other models on that account may wait too, but a limited model or scope is never picked again before its real deadline.
- When every key is cooling down, Orqen answers 429 with an exact `Retry-After` (seconds, rounded up) until the earliest key recovers. When it relays the last attempt's upstream 429, 402 or 401 while every key is blocked, it overwrites `Retry-After` with the pool-wide value.
- Cooldown state lives in memory and resets on restart.

## Retries and the commit boundary

- A request is retried only when all of these hold: the provider answered 401, 402 or 429; Orqen has not yet sent response headers to the client; the attempt count is below `max_attempts`; an eligible untried key remains; and Orqen is not shutting down. The retry sends the same body, and so the same model, on a different key.
- Nothing is retried after response headers go out. If the stream fails or goes idle after that point, Orqen aborts the client stream. It never splices output from another attempt.
- Connection failures, header timeouts, and other 4xx and 5xx responses are not retried, because the provider may already have received the request. Clients are expected to retry 5xx themselves.
- Every attempt runs inside the request handler future. If the client disconnects, the in-flight upstream request is dropped and no new attempt starts. There are no detached retry tasks. Orqen cannot cancel execution or billing for a request the provider already accepted.
- Response headers `x-orqen-key` (public key ID) and `x-orqen-attempts` show which key answered and on which attempt. Errors generated by Orqen itself carry `x-orqen-error: 1`.

## Limits and timeouts

| Setting | Default | Meaning |
| --- | --- | --- |
| `limits.max_request_bytes` | 32 MiB | request body cap (413) |
| `limits.request_read_timeout_secs` | 60 | time to receive the request body (408) |
| `upstream.connect_timeout_secs` | 10 | upstream connect |
| `upstream.response_header_timeout_secs` | 600 | wait for upstream response headers (504); non-streaming requests get headers only after completion |
| `upstream.stream_idle_timeout_secs` | 300 | longest silence between body chunks |
| `limits.shutdown_drain_secs` | 30 | wait for in-flight responses on shutdown |

## Health check

`GET /healthz` returns the version, uptime, upstream base URL, the ephemeral session count, and for each key its public ID, scope, credential and scope cooldowns, and per-model cooldowns. It answers 503 `draining` during shutdown.

## Shutdown

On SIGINT or SIGTERM, Orqen stops accepting connections, refuses new attempts with 503, and switches `/healthz` to 503 `draining`. In-flight responses get up to `shutdown_drain_secs`; remaining connections are then closed. Exiting the process drops every connection task and upstream request.

## Logging

`ORQEN_LOG` sets the level using `tracing` env-filter syntax (default `info`); output goes to stderr. Key values, key file contents, and raw conversation IDs are never logged, and keys are held only as sensitive header values. Log records carry the route, model, public key ID and scope, status code, attempt number, cooldown, and conversation hash.

## Running as a systemd user service

Follow the install comments in [`deploy/systemd/orqen.service`](../deploy/systemd/orqen.service). Keep `TimeoutStopSec` above `shutdown_drain_secs`.
