# Calling 1router from a client or AI agent

Base URL is `https://1router.hbstudio.space` (or `http://<host>:8080` on the
local network). Three protocols: OpenAI Chat Completions (`/v1/chat/completions`),
OpenAI Responses (`/v1/responses`, what Codex CLI speaks) and Anthropic Messages
(`/v1/messages`), plus `/v1/models` and `/v1/images/generations`. Use a **pool id**
(`GET /v1/models`) as `model`; chat pools and image pools are separate, so a chat call to an image pool fails with
"unknown model or pool".

Pick the credential you were given:

1. **1router key** (`1r_...`, from the admin's Users page): send
   `Authorization: Bearer 1r_...` (OpenAI-style) or `x-api-key: 1r_...`
   (Anthropic-style).
2. **Cloudflare Access service token** (a Client ID ending in `.access` plus
   a secret): send both headers on every request, in addition to a normal
   body:
   `CF-Access-Client-Id: <id>` and `CF-Access-Client-Secret: <secret>`.
   Requests are logged as `cf:<client id>`. Behind the Cloudflare hostname
   the tunnel will reject a request that lacks them (302 to a login page),
   so a client that only sends an API key can't get through there unless
   it also sends these headers.
3. **A person in a browser** signs in through Cloudflare Access with their
   email; this is for the admin UI, not for programs.

```
curl https://1router.hbstudio.space/v1/chat/completions   -H "CF-Access-Client-Id: $CF_ID" -H "CF-Access-Client-Secret: $CF_SECRET"   -H 'Content-Type: application/json'   -d '{"model":"vision","messages":[{"role":"user","content":"hi"}]}'
```

OpenAI Python SDK (any client that lets you set a base URL and extra headers
works the same way):

```python
from openai import OpenAI
client = OpenAI(
    base_url="https://1router.hbstudio.space/v1",
    api_key="unused-or-1r_key",          # required by the SDK; 1router ignores it when CF headers are valid
    default_headers={"CF-Access-Client-Id": CF_ID, "CF-Access-Client-Secret": CF_SECRET},
)
print(client.chat.completions.create(model="vision", messages=[{"role": "user", "content": "hi"}]))
```

Anthropic SDK / Claude-style agents: `Anthropic(base_url="https://1router.hbstudio.space", auth_token=..., default_headers={...})`
and a pool id as `model` (any chat pool works over any of the three protocols). Treat the
service-token secret like a password: keep it in an environment variable or
secret store, never in a repo, and ask the admin to revoke it if it leaks.

## Claude Code

Claude Code only takes a base URL and a token, so pass the Cloudflare Access
service-token headers through `ANTHROPIC_CUSTOM_HEADERS` (one `Name: Value`
per line):

```
export ANTHROPIC_BASE_URL=https://1router.hbstudio.space
export ANTHROPIC_AUTH_TOKEN=unused          # or your 1r_ key; any value works when the CF headers are valid
export ANTHROPIC_CUSTOM_HEADERS=$'CF-Access-Client-Id: <id>.access
CF-Access-Client-Secret: <secret>'
claude --model <chat-pool-id>               # e.g. command-code
```

- `ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer ...`;
  `ANTHROPIC_API_KEY` as `x-api-key`. 1router reads both. A real `1r_` key,
  if you set one, wins over the Cloudflare identity.
- Setting either one (or `ANTHROPIC_BASE_URL` with its own credential)
  replaces the claude.ai login for that session, so the claude.ai
  **connectors** (the account-level MCP servers such as Gmail or Drive) are
  disabled in it. MCP servers you configured yourself are unaffected. Use a
  separate terminal or profile for 1router sessions.
- Claude Code prints `unrecognized_model` for a pool id it doesn't know.
  It is only a warning; the request still goes through.
- Tested end to end: the request is logged as `cf:<client id>`.

## Codex CLI

Codex CLI only speaks the Responses API, which 1router serves at
`POST /v1/responses`. In `~/.codex/config.toml` (or a separate `CODEX_HOME`):

```toml
model = "<chat-pool-id>"
model_provider = "r1"

[model_providers.r1]
name = "1router"
base_url = "https://1router.hbstudio.space/v1"
wire_api = "responses"
env_key = "R1_KEY"                      # any value if open access is on
env_http_headers = { "CF-Access-Client-Id" = "CF_ID", "CF-Access-Client-Secret" = "CF_SECRET" }
```

- Supported: text and image input, `instructions`, function tools and their
  outputs, namespaced tools (flattened to `<namespace>__<tool>` upstream and
  restored on the way back), streaming, usage. Reasoning items and
  non-function tools (`web_search`, freeform/custom tools) are dropped.
- Any chat pool works, whatever wire format its providers speak.
- Run `codex exec` with stdin redirected (`< /dev/null`) in scripts.
- Tested end to end through the Cloudflare hostname with a service token.
