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
   and click **測試連線** (test connection).
3. On the **模型** (models) tab, pick the default model (and optionally the
   opus/sonnet/haiku mappings), then save. Changes apply immediately.
4. On the **使用方式** (usage) tab, click **安裝命令列工具** (install CLI), then:

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

* Codex against `anthropic`-kind providers (Claude Code works with both kinds).
* Recovering a stream that breaks midway. Today, fallback happens only
  before the first byte.
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
