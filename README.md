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
  is not touched. For Codex 0.163 and later it also writes a model catalog
  (`codex-models.json` next to the config file) so Codex knows Ferry's models
  instead of warning that their metadata is missing; `FERRY_CODEX_CATALOG=0`
  turns that off.
* The gateway turns both into OpenAI Chat Completions (or passes Messages
  straight through to Anthropic-compatible providers). It also converts the
  streamed answer back, including reasoning (`reasoning_content` or
  `<think>` tags), tool calls, and usage.
* Requests for `opus` / `sonnet` / `haiku` map to configured models. On an
  error before streaming starts, retryable errors are retried and then the
  configured fallbacks are tried in order.
* Housekeeping requests are answered locally: interactive Claude Code asks
  the model for a session title on every turn, and Ferry replies itself
  instead (a quota probe from older versions is handled the same way). Both
  can be switched off under `[optimizations]` or in the app's Settings.
* Claude Code's auto mode has the model review risky actions (Bash, writes
  outside the project) before running them, and blocks the action when the
  reply holds no verdict. Ferry gives these review requests enough output
  tokens for reasoning models and drops the verdict stop sequence some
  providers reject. When a model states the verdict another way
  (`shouldBlock: false`, `Block: no`) or only in its reasoning, Ferry adds
  the tag it meant; the Requests page shows each review's verdict.
  `ferry claude` also sets `CLAUDE_CODE_AUTO_MODE_SERVER=0`,
  because the server-side review only exists on Anthropic's API. A weak model
  can still block actions it should allow; map `sonnet` (which the review
  uses) to a capable model, or leave auto mode with Shift+Tab.
* Web search works on every provider. Claude Code's WebSearch and Codex's
  `web_search` are hosted tools that only OpenAI's and Anthropic's servers
  run; Ferry offers the model a `web_search` function instead, runs the
  search itself (DuckDuckGo by default, or Brave Search or a SearXNG
  instance), shows the client the search in its own format, and lets the
  model continue with the results. Searches go out from your machine; turn
  this off under `[web_search]` or in the app's Settings. (Claude Code's
  WebFetch already runs on your machine and needs nothing from Ferry.)
* Every provider kind serves both clients: Claude Code and Codex requests are
  converted to whatever API the target provider speaks.
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
context_window = 128000        # what Codex is told; 128000 when unset

[web_search]
enabled = true
backend = "duckduckgo"         # or "brave" (api_key) or "searxng" (url)
max_results = 8
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

* Signed and notarized macOS builds. The CI build is ad-hoc signed, so on
  first launch use right-click → Open.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
