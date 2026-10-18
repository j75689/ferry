# Ferry

Run the **native** Claude Code and Codex CLIs on any OpenAI-compatible or
Anthropic-compatible model provider. Ferry is written in Rust + Tauri and
ships as one desktop installer with no Python, Node, or other runtime to
manage. You only need the `claude` / `codex` CLIs themselves.

> Status: early prototype. Claude Code and Codex work end to end (streaming,
> thinking, tool calls) against OpenAI-compatible providers.

## How it works

```
claude ──ANTHROPIC_BASE_URL──▶ ┌──────────────────────────┐      ┌────────────────────┐
                               │ ferry gateway (localhost)  │      │ OpenAI-compatible  │
                               │  /v1/messages  (Claude)  │ ───▶ │ /chat/completions  │
codex ──model_providers.ferry──▶ │  /v1/responses (Codex)   │      │ or Anthropic API   │
                               │  route · retry · fallback│      └────────────────────┘
                               └──────────────────────────┘
```

* **Claude Code** speaks the Anthropic Messages API. `ferry claude` sets
  `ANTHROPIC_BASE_URL` / `ANTHROPIC_AUTH_TOKEN` for the child process only.
* **Codex** speaks the OpenAI Responses API. `ferry codex` adds a temporary
  `model_providers.ferry` entry through `-c` overrides. Your `~/.codex/config.toml`
  is not touched.
* The gateway turns both into OpenAI Chat Completions (or passes Messages
  straight through to Anthropic-compatible providers). It also converts the
  streamed answer back, including reasoning (`reasoning_content` or
  `<think>` tags), tool calls, and usage.
* Requests for `opus` / `sonnet` / `haiku` map to configured models. On an
  error before streaming starts, retryable errors are retried and then the
  configured fallbacks are tried in order.
* Every provider kind serves both clients: Claude Code and Codex requests are
  converted to OpenAI Chat, Anthropic Messages, or ACP as the target needs.
  Claude Code requests to Anthropic-type providers are passed through as-is.
* If a stream breaks after the client has already received part of the
  answer (connection reset, idle timeout, or a body that ends without a
  finish), Ferry reopens the request with the partial answer replayed and
  asks the model to continue. It tries the same model first, then the
  fallbacks, up to two times. Text the model repeats at the seam is trimmed,
  so the client sees one uninterrupted answer. Tool calls are only sent
  once complete, so they never arrive half-written.

## Layout

| Path | What |
|---|---|
| `crates/ferry-proxy` | The gateway library: protocol conversion, routing, upstream client, axum server |
| `crates/ferry-cli` | `ferry` binary: `ferry claude`, `ferry codex`, `ferry serve`, `ferry env` |
| `app/src-tauri` | Desktop app: embedded gateway, tray icon, settings commands |
| `app/ui` | Settings UI (plain HTML/CSS/JS, no build step) |

## Use

1. Install the desktop app (CI builds `.dmg`, `.msi`/`.exe`, `.deb`/`.AppImage`).
2. Open **Providers**, add one (for example NVIDIA NIM), paste the API key,
   and click **Test connection**.
3. On the **Models** tab, pick the default model (and optionally the
   opus/sonnet/haiku mappings), then save. Changes apply immediately.
4. On the **Usage** tab, click **Install CLI**, then:

```bash
ferry claude                 # interactive Claude Code
ferry codex                  # interactive Codex
ferry codex exec "fix it"    # every argument is passed through
```

`ferry` reuses the desktop app's gateway when it is running. Otherwise it
starts a private gateway on a free port for as long as the tool runs, so the
CLI works on its own too.

If you prefer to wire things up yourself, run `ferry env claude` or
`ferry env codex` to print the variables and config. The same snippets are on
the usage tab.

## Google Antigravity (subscription)

Ferry can use a Google Antigravity subscription through Google's official
ACP server (`agy_acp_server`, listed by Google LLC in the
[ACP registry](https://github.com/agentclientprotocol/registry)):

1. In **Providers**, add *Google Antigravity*, then click **Install**. Ferry
   downloads the build for your platform from `dl.google.com` into its data
   directory. **Update** appears when the registry has a newer version.
2. Click a sign-in method (for example *Sign in with Google*). The ACP server
   runs the sign-in itself and keeps the credentials; Ferry never sees them.
3. The model list is fetched after sign-in. Pick one as a route, for example
   `antigravity/<model>`, and save.

How it is served: each request opens a fresh agent session. The conversation
is rendered as a transcript, and the client's tools (Claude Code's or
Codex's) are offered to the agent through a loopback MCP server. When the
agent calls one, Ferry ends the turn and returns it to the client as a normal
tool call; the result goes back with the next request. The agent's own tools
are refused, and it runs in an empty working directory.

Caveats: this uses the agent as a model, which Google does not document as a
supported use. Review Google's terms before relying on it. The server
reports no reasoning details for tool turns, adds its own instructions on
top of the client's, and takes a few seconds to cold-start.

For development, `cargo run -p ferry-proxy --example mock_acp_server` speaks
the same protocol; `FERRY_ACP_REGISTRY_URL` and `FERRY_DATA_DIR` point the
installer at a local registry and data directory.

## Config

One TOML file, shared by the app and the CLI. The path comes from `$FERRY_CONFIG`,
or else the platform config dir is used (`ferry config` prints it). It is
reloaded automatically when it changes.

```toml
[server]
port = 8787
auth_token = "ferry-…"           # what Claude Code / Codex send to Ferry

[models]
default = "nim/meta/llama-3.3-70b-instruct"
haiku = "groq/openai/gpt-oss-120b"
fallbacks = ["openrouter/deepseek/deepseek-chat-v3.1:free"]

[[providers]]
id = "nim"
kind = "openai_chat"           # or "anthropic"
base_url = "https://integrate.api.nvidia.com/v1"
api_key = "nvapi-…"            # or api_key_env = "NVIDIA_API_KEY"
max_tokens_cap = 16384         # clamp Claude Code's 32k default for free tiers
reasoning_history = false      # true for DeepSeek-style thinking + tools
models = ["meta/llama-3.3-70b-instruct"]
```

## Develop

```bash
cargo test                                  # proxy + CLI (no WebView needed)
cargo run -p ferry-proxy --example mock_provider -- 9999   # fake provider, no key
cargo run -p ferry-cli -- serve               # gateway in the foreground
cargo run -p ferry-desktop                    # desktop app (needs WebView libs)
```

On Linux, the desktop app needs `libwebkit2gtk-4.1-dev
libayatana-appindicator3-dev librsvg2-dev`.

To build installers locally:

```bash
cargo build --release -p ferry-cli
mkdir -p app/src-tauri/binaries
cp target/release/ferry app/src-tauri/binaries/ferry-$(rustc -vV | sed -n 's/host: //p')
cd app/src-tauri && cargo tauri build --config tauri.release.conf.json
```

## Not yet

* Local short-circuits for Claude Code's background requests (titles,
  quota probes); they currently go to the configured haiku model.
* Server-side tools (`web_search`, `web_fetch`) are dropped rather than
  emulated.
* Codex prints a "model metadata not found" notice for non-OpenAI model names.
* Signed and notarized macOS builds. The CI build is ad-hoc signed, so on
  first launch use right-click → Open.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
