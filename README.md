# 1router

Your own LLM API gateway. Point Claude Code, Cursor, OpenCode, or any
OpenAI/Anthropic-compatible tool at one URL, and 1router routes each
request to whichever provider you've configured — OpenAI, Anthropic,
DeepSeek, OpenCode, a ChatGPT account, or a Command Code account. One
binary, one local database file, no external services to run.

## Install

**Prebuilt binary** (recommended) — grab the latest from the
[Releases page](https://github.com/ducphamhoang/1router/releases/latest):

```
curl -LO https://github.com/ducphamhoang/1router/releases/latest/download/1router-<version>-<target>.tar.gz
tar -xzf 1router-<version>-<target>.tar.gz
```

`<target>` is one of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`x86_64-apple-darwin`, `aarch64-apple-darwin` — match your OS/CPU. `<version>`
is the version shown on the Releases page (e.g. `v0.3.3`).

**Docker**:

```
mkdir -p data
docker run -it --rm -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db ghcr.io/ducphamhoang/1router:latest setup
docker run -d --name 1router -p 8080:8080 -v "$PWD/data:/data" \
  -e ROUTER_SQLITE_PATH=/data/1router.db ghcr.io/ducphamhoang/1router:latest
```

**From source** (needs Rust + Node.js):

```
cargo build --release
```

## Set up

Run the interactive wizard once:

```
./1router setup
```

It walks you through:

1. **An admin secret** — used to call the API and log in to the dashboard.
2. **Adding a provider** — pick one:
   - **OpenAI**, **Anthropic**, **DeepSeek**, or **OpenCode** — paste an
     API key (OpenCode has a free tier that needs no key at all).
   - **ChatGPT account (Codex)** or **Command Code** — logs in through
     your browser, no API key needed.
3. **Making it callable** — the wizard names it and makes it immediately
   usable as a model.

Then start the server:

```
./1router
```

The same wizard also runs automatically the first time you start 1router
with an empty database.

## Try it

```
curl http://localhost:8080/v1/chat/completions \
  -H "Authorization: Bearer $(cat .router_secret)" \
  -H 'Content-Type: application/json' \
  -d '{"model":"<pool-id>","messages":[{"role":"user","content":"hi"}]}'
```

Replace `<pool-id>` with whatever you named the provider during setup —
that's the value you put in `model`, from this `curl` example or from any
client you point at `http://localhost:8080`.

## Admin dashboard

Open `http://localhost:8080/ui/` and log in as `admin` with the password
from setup.

**Providers** — add, edit, or remove providers. Picking a template
pre-fills the wire format, base URL, and a starting model, so you don't
need to look up API details yourself.

![Providers page](docs/screenshots/providers.png)
![New provider dialog](docs/screenshots/new-provider.png)

**Pools** — group providers under one name for round-robin/failover, or
serve several models from one credential.

![Pools page](docs/screenshots/pools.png)

**Settings** — everything needed to connect a client (base URL, secret,
copy-pasteable curl example), plus a button to check which models each
provider actually offers right now.

![Settings page](docs/screenshots/settings.png)

*(Screenshots not included yet — see
[`docs/screenshots/README.md`](docs/screenshots/README.md) if you'd like
to add them.)*

## Forgot your admin password?

```
./1router setup --reset-admin-password
```

For headless deployments (Docker, systemd), set `ROUTER_SHARED_SECRET`
yourself instead of relying on the interactive wizard — see
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#configuration-environment-variables)
for the full list of environment variables.

## Learn more

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — configuration reference,
  wire-format translation rules, the admin API, and how a request flows
  through the system
- [`CLAUDE.md`](CLAUDE.md) — building from source, running tests,
  contributor conventions
