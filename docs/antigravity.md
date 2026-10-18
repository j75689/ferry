# Google Antigravity provider

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
tool call; the result goes back with the next request. When the agent asks
for permission to call one of these client tools, Ferry allows it, since the
call only reaches the client, which applies its own permission checks. The
agent's own tools are refused, and it runs in an empty working directory.

If every tool call fails with `tool call denied by pre-tool hook`, look at
`~/.gemini/config/hooks.json` (`%USERPROFILE%\.gemini\config\hooks.json`
on Windows). The ACP server runs the `PreToolUse` hooks that other tools
install there, and a hook that answers `{}` instead of an explicit decision
denies every call. Remove that hook, or have it answer `{"decision": "ask"}`.

The ACP server can get stuck failing every session with `Internal error
(code -32603)`. Ferry retries such a failure once, then restarts the server
and tries again; the error it reports includes the server's details.

Caveats: this uses the agent as a model, which Google does not document as a
supported use. Review Google's terms before relying on it. The server
reports no reasoning details for tool turns, adds its own instructions on
top of the client's, and takes a few seconds to cold-start.

For development, `cargo run -p ferry-proxy --example mock_acp_server` speaks
the same protocol; `FERRY_ACP_REGISTRY_URL` and `FERRY_DATA_DIR` point the
installer at a local registry and data directory.
