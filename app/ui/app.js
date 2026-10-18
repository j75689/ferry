"use strict";

// Inside Tauri this is the real backend; a browser preview may inject
// `window.Ferry_DEV_BACKEND` with the same command names.
const invoke = window.__TAURI__
  ? window.__TAURI__.core.invoke
  : (cmd, args) => window.Ferry_DEV_BACKEND(cmd, args);

const $ = (id) => document.getElementById(id);

let snapshot = null;
let draft = null;
let selected = null; // index into draft.providers
let dirty = false;

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (value === true) node.setAttribute(key, "");
    else if (value !== false && value != null) node.setAttribute(key, value);
  }
  for (const child of children.flat()) {
    if (child == null) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function toast(message, ms = 2600) {
  const node = $("toast");
  node.textContent = message;
  node.classList.remove("hidden");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add("hidden"), ms);
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast("Copied");
  } catch {
    toast("Clipboard is not available");
  }
}

function markDirty() {
  dirty = true;
  document.querySelectorAll("[data-save]").forEach((b) => (b.textContent = "Save •"));
}

function clearDirty() {
  dirty = false;
  document.querySelectorAll("[data-save]").forEach((b) => (b.textContent = "Save"));
}

// ---------- status ----------

function renderStatus() {
  const dot = $("status-dot");
  dot.className = "dot " + (snapshot.running ? "ok" : "bad");
  $("status-text").textContent = snapshot.running ? `Running at ${snapshot.url}` : "Gateway stopped";
  const banner = $("banner");
  const problems = [];
  if (snapshot.error) problems.push(snapshot.error);
  if (!snapshot.config.models.default) problems.push("No default model yet: add a provider under Providers, then pick a model under Models.");
  banner.textContent = problems.join("\n");
  banner.classList.toggle("hidden", problems.length === 0);
}

// ---------- models ----------

function modelOptions() {
  const options = new Set(snapshot.advertised_models);
  for (const p of draft.providers) for (const m of p.models || []) options.add(`${p.id}/${m}`);
  return [...options];
}

function renderModels() {
  const m = draft.models;
  $("m-default").value = m.default || "";
  $("m-opus").value = m.opus || "";
  $("m-sonnet").value = m.sonnet || "";
  $("m-haiku").value = m.haiku || "";
  $("m-fallbacks").value = (m.fallbacks || []).join("\n");
  const list = $("model-options");
  list.replaceChildren(...modelOptions().map((value) => el("option", { value })));
}

function bindModels() {
  const optional = (v) => (v.trim() ? v.trim() : null);
  $("m-default").addEventListener("input", (e) => { draft.models.default = e.target.value.trim(); markDirty(); });
  for (const tier of ["opus", "sonnet", "haiku"]) {
    $(`m-${tier}`).addEventListener("input", (e) => { draft.models[tier] = optional(e.target.value); markDirty(); });
  }
  $("m-fallbacks").addEventListener("input", (e) => {
    draft.models.fallbacks = e.target.value.split("\n").map((s) => s.trim()).filter(Boolean);
    markDirty();
  });
}

// ---------- providers ----------

function renderProviderList() {
  const list = $("provider-list");
  list.replaceChildren(
    ...draft.providers.map((p, index) =>
      el("li", { class: index === selected ? "active" : "", onclick: () => { selected = index; renderProviders(); } },
        el("span", {}, p.id || "(unnamed)"),
        el("span", { class: "kind" }, p.kind === "anthropic" ? "Anthropic" : "OpenAI")),
    ),
  );
  if (draft.providers.length === 0) list.append(el("li", { class: "empty" }, "No providers yet"));
}

function field(label, input, hint) {
  return el("label", {}, label, input, hint ? el("span", { class: "hint" }, hint) : null);
}

function renderEditor() {
  const box = $("provider-editor");
  const p = draft.providers[selected];
  if (!p) {
    box.replaceChildren(el("p", { class: "empty" }, "Select a provider on the left, or add one."));
    return;
  }
  const preset = snapshot.presets.find((x) => x.id === p.id);
  const update = (key, transform = (v) => v) => (e) => {
    p[key] = transform(e.target.type === "checkbox" ? e.target.checked : e.target.value);
    markDirty();
    if (key === "id" || key === "kind") renderProviderList();
  };
  const result = el("div", { class: "result" });
  const modelsArea = el("textarea", { rows: "4", oninput: update("models", (v) => v.split("\n").map((s) => s.trim()).filter(Boolean)) });
  modelsArea.value = (p.models || []).join("\n");

  const fetchButton = el("button", { class: "ghost small", onclick: async (e) => {
    e.target.disabled = true;
    result.className = "result";
    result.textContent = "Loading the model list…";
    try {
      const ids = await invoke("fetch_models", { provider: p });
      result.className = "result ok";
      result.textContent = `Found ${ids.length} models (added to the model suggestions; paste the ones you want above).`;
      const known = new Set(snapshot.advertised_models);
      ids.forEach((id) => known.add(`${p.id}/${id}`));
      snapshot.advertised_models = [...known];
      renderModels();
    } catch (err) {
      result.className = "result bad";
      result.textContent = String(err);
    } finally {
      e.target.disabled = false;
    }
  } }, "Fetch models");

  const testButton = el("button", { class: "ghost small", onclick: async (e) => {
    const model = (p.models || [])[0] || "";
    e.target.disabled = true;
    result.className = "result";
    result.textContent = `Testing with ${model || "(none)"}…`;
    try {
      result.textContent = await invoke("test_provider", { provider: p, model });
      result.className = "result ok";
    } catch (err) {
      result.className = "result bad";
      result.textContent = String(err);
    } finally {
      e.target.disabled = false;
    }
  } }, "Test connection");

  const input = (key, attrs = {}) => {
    const node = el("input", { ...attrs, oninput: update(key, attrs.type === "number" ? (v) => (v === "" ? null : Number(v)) : (v) => v) });
    node.value = p[key] ?? "";
    return node;
  };
  const check = (key, label, hint) => {
    const node = el("input", { type: "checkbox", onchange: update(key) });
    node.checked = !!p[key];
    return el("label", { class: "check", title: hint }, node, label);
  };
  const kind = el("select", { onchange: update("kind") },
    el("option", { value: "openai_chat" }, "OpenAI-compatible (/chat/completions)"),
    el("option", { value: "anthropic" }, "Anthropic-compatible (/v1/messages, Claude Code only)"));
  kind.value = p.kind || "openai_chat";

  box.replaceChildren(
    el("div", { class: "row-between" },
      el("h2", {}, p.id || "New provider"),
      preset && preset.key_url ? el("button", { class: "ghost small", onclick: () => openUrl(preset.key_url) }, "Get an API key ↗") : null),
    el("div", { class: "grid" },
      field("ID (prefix in model names)", input("id", { placeholder: "nim" })),
      field("Type", kind)),
    field("Base URL", input("base_url", { placeholder: "https://…/v1", class: "mono" })),
    el("div", { class: "grid" },
      field("API key", input("api_key", { type: "password", autocomplete: "off", placeholder: "Leave empty for local models" })),
      field("Or read it from an environment variable", input("api_key_env", { placeholder: "e.g. NVIDIA_API_KEY" }))),
    field("Models (one per line, without the provider prefix; the first is used for testing)", modelsArea),
    el("div", { class: "actions" }, fetchButton, testButton),
    result,
    el("div", { class: "grid" },
      field("Max output tokens", input("max_tokens_cap", { type: "number", min: "1", placeholder: "No limit" }), "Free tiers often limit output length; Claude Code asks for 32k by default.")),
    check("reasoning_history", "Replay earlier reasoning (reasoning_content)", "Needed by DeepSeek-style thinking models around tool calls"),
    check("send_reasoning_effort", "Forward reasoning effort (reasoning_effort)"),
    check("stream_usage", "Request streamed usage (stream_options)"),
    el("div", { class: "actions" },
      el("button", { class: "danger", onclick: () => {
        if (!confirm(`Delete provider "${p.id}"?`)) return;
        draft.providers.splice(selected, 1);
        selected = draft.providers.length ? 0 : null;
        markDirty();
        renderProviders();
      } }, "Delete"),
      el("button", { class: "primary", "data-save": true }, dirty ? "Save •" : "Save")),
  );
}

function renderProviders() {
  renderProviderList();
  renderEditor();
}

function addProvider() {
  const preset = snapshot.presets.find((x) => x.id === $("preset-select").value);
  let id = preset.id;
  for (let n = 2; draft.providers.some((p) => p.id === id); n++) id = `${preset.id}${n}`;
  draft.providers.push({
    id,
    kind: preset.kind,
    base_url: preset.base_url,
    api_key: "",
    max_tokens_cap: preset.max_tokens_cap,
    send_reasoning_effort: false,
    reasoning_history: preset.reasoning_history,
    stream_usage: true,
    models: preset.example_model ? [preset.example_model] : [],
  });
  selected = draft.providers.length - 1;
  if (!draft.models.default && preset.example_model) draft.models.default = `${id}/${preset.example_model}`;
  markDirty();
  renderProviders();
  renderModels();
}

function openUrl(url) {
  if (window.__TAURI__) invoke("plugin:opener|open_url", { url });
  else window.open(url, "_blank");
}

// ---------- usage ----------

function renderUsage() {
  const c = snapshot.config;
  const url = snapshot.url;
  $("claude-env").textContent =
    `# macOS / Linux\nexport ANTHROPIC_BASE_URL=${url}\nexport ANTHROPIC_AUTH_TOKEN=${c.server.auth_token}\nunset ANTHROPIC_API_KEY\nclaude\n\n` +
    `# Windows PowerShell\n$env:ANTHROPIC_BASE_URL="${url}"\n$env:ANTHROPIC_AUTH_TOKEN="${c.server.auth_token}"\nclaude`;
  $("codex-env").textContent =
    `model_provider = "ferry"\nmodel = ${JSON.stringify(c.models.default || "provider/model")}\n\n` +
    `[model_providers.ferry]\nname = "Ferry"\nbase_url = "${url}/v1"\nwire_api = "responses"\nenv_key = "FERRY_API_KEY"\n\n` +
    `# and in your shell: export FERRY_API_KEY=${c.server.auth_token}`;
}

// ---------- requests ----------

async function refreshRequests() {
  if (!$("tab-requests").classList.contains("active")) return;
  const rows = await invoke("recent_requests");
  $("req-count").textContent = `${rows.length} requests`;
  $("req-body").replaceChildren(
    ...rows.map((r) => {
      const ok = r.outcome.startsWith("ok");
      return el("tr", {},
        el("td", {}, new Date(r.started_at_ms).toLocaleTimeString()),
        el("td", {}, r.api === "messages" ? "Claude" : "Codex"),
        el("td", {}, r.requested_model),
        el("td", {}, r.routed_model),
        el("td", { class: ok ? "outcome-ok" : "outcome-bad" }, r.outcome),
        el("td", {}, `${(r.duration_ms / 1000).toFixed(1)}s`),
        el("td", {}, `${r.input_tokens} / ${r.output_tokens}`));
    }),
  );
}

// ---------- settings ----------

function renderSettings() {
  const s = draft.server;
  $("s-host").value = s.host;
  $("s-port").value = s.port;
  $("s-timeout").value = s.connect_timeout_secs;
  $("s-retries").value = s.max_retries;
  $("s-token").value = s.auth_token;
  $("config-path").textContent = snapshot.config_path;
}

function bindSettings() {
  const num = (key) => (e) => { draft.server[key] = Number(e.target.value); markDirty(); };
  $("s-host").addEventListener("input", (e) => { draft.server.host = e.target.value.trim(); markDirty(); });
  $("s-port").addEventListener("input", num("port"));
  $("s-timeout").addEventListener("input", num("connect_timeout_secs"));
  $("s-retries").addEventListener("input", num("max_retries"));
  $("s-token").addEventListener("input", (e) => { draft.server.auth_token = e.target.value.trim(); markDirty(); });
  $("regen-token").addEventListener("click", async () => {
    draft.server.auth_token = await invoke("new_token");
    $("s-token").value = draft.server.auth_token;
    markDirty();
  });
  $("restart").addEventListener("click", async () => {
    apply(await invoke("restart_gateway"), false);
    toast(snapshot.running ? "Gateway restarted" : "Gateway failed to start");
  });
}

// ---------- lifecycle ----------

function apply(next, resetDraft = true) {
  snapshot = next;
  if (resetDraft || !draft) {
    draft = structuredClone(next.config);
    if (selected != null && selected >= draft.providers.length) selected = null;
    clearDirty();
  }
  renderStatus();
  renderModels();
  renderProviders();
  renderUsage();
  renderSettings();
}

async function save() {
  try {
    apply(await invoke("save_config", { config: draft }));
    toast("Saved; changes apply immediately");
  } catch (err) {
    toast(`Save failed: ${err}`);
  }
}

function bindTabs() {
  document.querySelectorAll(".tabs button").forEach((button) =>
    button.addEventListener("click", () => {
      document.querySelectorAll(".tabs button").forEach((b) => b.classList.toggle("active", b === button));
      document.querySelectorAll(".panel").forEach((p) => p.classList.toggle("active", p.id === `tab-${button.dataset.tab}`));
      refreshRequests();
    }),
  );
}

async function main() {
  bindTabs();
  bindModels();
  bindSettings();
  $("preset-select").replaceChildren();
  document.addEventListener("click", (e) => {
    const target = e.target.closest("[data-save]");
    if (target) save();
    const copyTarget = e.target.closest("[data-copy]");
    if (copyTarget) copy($(copyTarget.dataset.copy).textContent);
  });
  $("copy-url").addEventListener("click", () => copy(snapshot.url));
  $("add-provider").addEventListener("click", addProvider);
  $("install-cli").addEventListener("click", async () => {
    try {
      toast(await invoke("install_cli"), 9000);
    } catch (err) {
      toast(`Install failed: ${err}`, 9000);
    }
  });
  window.addEventListener("beforeunload", (e) => { if (dirty) e.preventDefault(); });
  document.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key === "s") {
      e.preventDefault();
      save();
    }
  });

  apply(await invoke("get_snapshot"));
  $("preset-select").replaceChildren(...snapshot.presets.map((p) => el("option", { value: p.id }, p.name)));
  setInterval(async () => {
    const next = await invoke("get_snapshot");
    // Never re-render forms under the user's cursor; only refresh the status.
    if (dirty) {
      snapshot = { ...next, config: snapshot.config, advertised_models: snapshot.advertised_models };
      renderStatus();
    } else if (JSON.stringify(next.config) !== JSON.stringify(snapshot.config) || next.running !== snapshot.running) apply(next);
    else { snapshot = next; renderStatus(); }
  }, 3000);
  setInterval(refreshRequests, 2000);
}

main().catch((err) => {
  document.body.prepend(el("div", { class: "banner" }, `The settings UI failed to start: ${err}`));
});
