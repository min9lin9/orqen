# Orqen

[![CI](https://github.com/min9lin9/orqen/actions/workflows/ci.yml/badge.svg)](https://github.com/min9lin9/orqen/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**코딩 에이전트를 위한 로컬 모델 프로바이더 게이트웨이.** [English](README.md)

Orqen은 내 컴퓨터에서 코딩 에이전트와 모델 프로바이더 사이에 놓이는 게이트웨이입니다. omo나 Claude Code처럼 OpenAI 또는 Anthropic 호환 API를 쓰는 클라이언트를 Orqen에 연결하면, Orqen이 요청을 여러 API 키에 나눠 보내고 같은 대화는 같은 키로 보내며 응답이 시작되기 전의 한도 초과를 처리합니다. 요청과 응답의 바이트는 바꾸지 않습니다.

아직 초기 버전(v0.1)입니다. 지금 지원하는 프로바이더는 [OpenCode Go](https://opencode.ai) 하나이고 프로바이더를 계속 늘려 갈 계획입니다. 자세한 내용은 [로드맵](ROADMAP.md)에 있습니다.

## 왜 Orqen인가

- **키는 여러 개, 엔드포인트는 하나.** 계정 하나든 여럿이든 키를 등록해 두면 에이전트는 로컬 주소 하나만 봅니다.
- **대화 고정.** 같은 대화는 계속 같은 키로 가므로 프로바이더 쪽 캐시를 살리기 좋습니다.
- **범위에 맞춘 한도 처리.** 429가 오면 설정에 따라 계정 전체나 해당 모델만 쉬게 합니다. 재시도는 다른 키로 하되, 클라이언트에 아직 아무것도 보내지 않았을 때만 합니다.
- **내용은 그대로.** 도구 호출, 추론 블록, SSE 스트림을 손대지 않고 전달합니다.

## 지원 기능

### 프로바이더

| 프로바이더 | 상태 |
| --- | --- |
| OpenCode Go | 지원 |
| OpenCode Zen, OpenRouter, z.ai, Kimi, MiniMax, Alibaba Coding Plan | 후보. [프로바이더 확장 설계](docs/provider-expansion.md#candidate-providers) 참고 |

### 클라이언트 API

| 경로 | 프로토콜 |
| --- | --- |
| `POST /v1/chat/completions` | OpenAI Chat Completions(스트리밍·도구 호출 포함) |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses` | OpenAI Responses |
| `GET /v1/models` | 모델 목록 |
| `GET /healthz` | 상태와 키별 cooldown 현황 |

### 키 풀

- 키는 하나씩 별도 파일에 두고, 키마다 계정 범위(scope)를 지정합니다.
- 401이 오면 그 자격 증명을, 402가 오면 계정 전체를 막습니다. 429는 계정 전체나 모델 하나를 막으며 `Retry-After`를 따릅니다.
- 응답 헤더를 보내기 전에 401·402·429가 오면 같은 모델로 다른 키에 다시 보냅니다.
- 5xx는 그대로 전달하고 재시도는 클라이언트에 맡깁니다.

### 대화 식별

- `x-opencode-session`, `x-omo-session`, `x-session-id`, `session_id` 헤더로 대화를 구분합니다. 목록은 설정에서 바꿀 수 있습니다.
- 식별 헤더가 없는 요청은 일회용 세션을 받거나(`ephemeral`) 거부됩니다(`reject`).

### 운영

- 설정은 TOML 파일 하나이고 비밀은 따로 둔 키 파일에만 들어갑니다.
- `orqen --check`로 시작 전에 설정과 키 파일을 검사합니다.
- 종료 신호를 받으면 진행 중인 응답을 마치고 내려갑니다. systemd 사용자 유닛이 함께 들어 있습니다.
- 로그에는 공개 키 ID와 대화 해시만 남고 비밀이나 원본 대화 ID는 남지 않습니다.

## 빠른 시작

Rust 1.88 이상과 OpenCode Go API 키가 필요합니다.

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

Orqen은 `127.0.0.1:8790`에서 요청을 받습니다. 다른 터미널에서 서버가 떴는지, 프로바이더에 닿는지 확인합니다.

```bash
curl -s http://127.0.0.1:8790/healthz
curl -s http://127.0.0.1:8790/v1/models
```

설정 항목 전체와 설명은 [`examples/orqen.example.toml`](examples/orqen.example.toml)에 있습니다.

## 클라이언트 연결

| 클라이언트 프로토콜 | Base URL |
| --- | --- |
| OpenAI 호환 | `http://127.0.0.1:8790/v1` |
| Anthropic 호환 | `http://127.0.0.1:8790` |

클라이언트의 API 키 자리에는 아무 값이나 넣어도 됩니다. Orqen이 풀에 있는 키로 바꿔 보냅니다. omo, Claude Code, OpenAI SDK, curl 설정 예시는 [docs/clients.md](docs/clients.md)에 있습니다.

## 문서

- [클라이언트 연결](docs/clients.md)
- [운영 가이드](docs/operations.md): 설정, 경로, 키 풀, 재시도, 한계값, 종료, 로그
- [설계 원칙](docs/design.md)
- [프로바이더 확장 설계](docs/provider-expansion.md)
- [프로바이더 추가 방법](docs/adding-a-provider.md)
- [로드맵](ROADMAP.md)
- [변경 기록](CHANGELOG.md)

`docs/` 아래 문서는 영어로 작성합니다.

## 알려진 한계

- 프로바이더는 OpenCode Go 하나입니다.
- 프로토콜 변환을 하지 않으므로, 클라이언트는 프로바이더가 제공하는 프로토콜로 요청해야 합니다.
- `POST /v1/messages/count_tokens`는 아직 없습니다.
- cooldown 상태는 메모리에만 있어서 재시작하면 초기화됩니다.

## 기여

버그 제보, 프로바이더 요청, 풀 리퀘스트 모두 환영합니다. [CONTRIBUTING.md](CONTRIBUTING.md)부터 읽어 주세요. 이 프로젝트는 [행동 강령](CODE_OF_CONDUCT.md)을 따릅니다. 보안 문제는 공개 이슈가 아니라 [SECURITY.md](SECURITY.md)의 방법으로 알려 주세요.

## 감사의 말

프로바이더 확장 계획은 [CodexBar](https://github.com/steipete/CodexBar)의 프로바이더 목록과 descriptor 설계를 참고했습니다.

## 라이선스

[MIT](LICENSE)
