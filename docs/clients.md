# Connecting clients

Orqen exposes OpenAI- and Anthropic-compatible routes on a loopback address, so any client that lets you set a base URL can use it. The examples assume the default `listen = "127.0.0.1:8790"`.

## What every client should know

- **API key:** send any placeholder. Orqen drops the client's `Authorization` and `x-api-key` headers and uses a key from its pool.
- **Model names:** pass the provider's model IDs unchanged. `GET /v1/models` lists them.
- **Conversation identity:** for key affinity and provider caching, send a stable per-conversation header that is listed in `session.headers`. The defaults are `x-opencode-session`, `x-omo-session`, `x-session-id` and `session_id`. If your client sends a different header, add its name to `session.headers`. Requests without one are handled by `session.missing`; see [operations.md](operations.md#conversation-identity).
- **Protocol:** Orqen does not translate between OpenAI and Anthropic formats. A client can use only the protocols the provider serves for the chosen model.
- **5xx responses** reach the client unchanged. Keep your client's own retry enabled.

## curl

```bash
curl -s http://127.0.0.1:8790/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'x-session-id: demo-1' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"Say hi"}]}'
```

The response headers `x-orqen-key` and `x-orqen-attempts` show which key answered and on which attempt.

## OpenAI SDK (Python)

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8790/v1", api_key="orqen-local")
reply = client.chat.completions.create(
    model="deepseek-v4.1-flash",
    messages=[{"role": "user", "content": "Say hi"}],
    extra_headers={"x-session-id": "demo-1"},
)
print(reply.choices[0].message.content)
```

## omo

Add a provider to omo's `models.json`. The `compat` settings make omo send a per-conversation `session_id` header, which Orqen reads by default.

```json
{
  "providers": {
    "orqen": {
      "name": "Orqen",
      "baseUrl": "http://127.0.0.1:8790/v1",
      "api": "openai-completions",
      "apiKey": "orqen-local",
      "compat": {
        "sendSessionAffinityHeaders": true,
        "sessionAffinityFormat": "openai"
      },
      "models": [
        {
          "id": "deepseek-v4.1-flash",
          "name": "DeepSeek V4.1 Flash via Orqen",
          "reasoning": true,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 8192
        }
      ]
    }
  }
}
```

```bash
omo --model orqen/deepseek-v4.1-flash
```

- Without the `compat` block omo sends no conversation header, and `session.missing` applies.
- `contextWindow` and `maxTokens` are client-side hints. Set them from the provider's published limits.
- omo's model discovery fills in models only for a provider that already exists in `models.json`, so add the provider entry first.

## Claude Code

Claude Code reads its endpoint from environment variables:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8790
export ANTHROPIC_AUTH_TOKEN=orqen-local
claude --model <model-id>
```

- Choose a model that the provider serves over Anthropic Messages.
- If Claude Code sends a per-session header, add its name to `session.headers` to get key affinity. Otherwise each request gets an ephemeral session.
- Orqen does not implement `POST /v1/messages/count_tokens`.

## Other clients

Any tool with a configurable OpenAI or Anthropic base URL works the same way. If you have set one up, a pull request adding it to this page is welcome.
