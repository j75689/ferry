"use strict";

// Inside Tauri this is the real backend; a browser preview may inject
// `window.FERRY_DEV_BACKEND` with the same command names.
const invoke = window.__TAURI__
  ? window.__TAURI__.core.invoke
  : (cmd, args) => window.FERRY_DEV_BACKEND(cmd, args);

const $ = (id) => document.getElementById(id);
const ICONS = window.FERRY_ICONS || {};
const SVG_NS = "http://www.w3.org/2000/svg";
const MODEL_REF = /^[^\s/]+\/\S+$/;
const PAGES = ["models", "providers", "usage", "requests", "settings"];
const KIND_LABELS = { openai_chat: "OpenAI", anthropic: "Anthropic", antigravity: "Antigravity" };

let snapshot = null;
let draft = null;
let selected = null; // index into draft.providers
let dirty = false;
let page = "models";

// ---------- building blocks ----------

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

/** A decorative icon; controls get their accessible name from text or aria-label. */
function icon(name) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 256 256");
  svg.setAttribute("class", "icon");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const d of ICONS[name] || []) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    svg.append(path);
  }
  return svg;
}

function decorateIcons(root = document) {
  root.querySelectorAll("[data-icon]").forEach((node) => {
    if (!node.querySelector(":scope > svg.icon")) node.prepend(icon(node.dataset.icon));
  });
}

function button(label, { variant = "", iconName = null, onclick = null, ariaLabel = null } = {}) {
  const node = el("button", { class: `button ${variant}`.trim(), type: "button", "aria-label": ariaLabel });
  if (iconName) node.append(icon(iconName));
  node.append(el("span", {}, label));
  if (onclick) node.addEventListener("click", onclick);
  return node;
}

/** Disable `node` and show a spinner while `work` runs; ignores repeat clicks. */
async function busy(node, work) {
  if (node.getAttribute("aria-busy") === "true") return;
  node.setAttribute("aria-busy", "true");
  node.disabled = true;
  const firstIcon = node.querySelector(":scope > svg.icon");
  const spinner = el("span", { class: "spinner", "aria-hidden": "true" });
  if (firstIcon) firstIcon.replaceWith(spinner);
  else node.prepend(spinner);
  try {
    return await work();
  } finally {
    spinner.replaceWith(firstIcon || "");
    node.disabled = false;
    node.removeAttribute("aria-busy");
  }
}

/** Show an inline outcome: "ok", "bad", or "busy". */
function message(node, kind, text) {
  node.className = `result ${kind}`;
  const lead = kind === "ok" ? icon("check-circle") : kind === "bad" ? icon("warning-circle") : el("span", { class: "spinner", "aria-hidden": "true" });
  node.replaceChildren(lead, el("span", {}, text));
}

function toast(text, kind = "ok", ms = 4000) {
  const node = $("toast");
  node.className = `toast ${kind}`;
  node.replaceChildren(icon(kind === "bad" ? "warning-circle" : "check-circle"), el("span", {}, text));
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add("hidden"), ms);
}

async function copy(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast("Copied to the clipboard");
  } catch {
    toast("The clipboard is not available", "bad");
  }
}

/** A labelled control with optional helper text and an error slot. */
function field({ id, label, control, help = null, required = false }) {
  control.id = id;
  const describedBy = [help ? `${id}-help` : null, `${id}-error`].filter(Boolean).join(" ");
  control.setAttribute("aria-describedby", describedBy);
  if (required) control.required = true;
  return el("div", { class: "field" },
    el("label", { for: id }, label, required ? el("span", { class: "req", "aria-hidden": "true" }, " *") : null),
    control,
    help ? el("p", { class: "help", id: `${id}-help` }, help) : null,
    el("p", { class: "error", id: `${id}-error` }));
}

function setError(control, text) {
  const slot = $(`${control.id}-error`);
  control.setAttribute("aria-invalid", text ? "true" : "false");
  if (slot) slot.replaceChildren(...(text ? [icon("warning-circle"), el("span", {}, text)] : []));
  return !text;
}

function markDirty() {
  dirty = true;
  document.querySelectorAll("[data-save]").forEach((b) => b.replaceChildren(el("span", { class: "unsaved", "aria-hidden": "true" }), "Save changes"));
}

function clearDirty() {
  dirty = false;
  document.querySelectorAll("[data-save]").forEach((b) => b.replaceChildren("Save"));
}

function saveButton() {
  const node = el("button", { class: "button primary", type: "button", "data-save": true });
  node.append(...(dirty ? [el("span", { class: "unsaved", "aria-hidden": "true" }), "Save changes"] : ["Save"]));
  return node;
}

// ---------- navigation ----------

function showPage(name, focus = true) {
  page = PAGES.includes(name) ? name : "models";
  for (const id of PAGES) $(`page-${id}`).hidden = id !== page;
  document.querySelectorAll(".nav a").forEach((a) => {
    if (a.dataset.page === page) a.setAttribute("aria-current", "page");
    else a.removeAttribute("aria-current");
  });
  if (focus) $(`h-${page}`).focus();
  refreshRequests();
}

function bindNavigation() {
  window.addEventListener("hashchange", () => showPage(location.hash.slice(1)));
  showPage(location.hash.slice(1), false);
}

// ---------- status ----------

function renderStatus() {
  const statusIcon = $("status-icon");
  statusIcon.className = `status-icon ${snapshot.running ? "ok" : "bad"}`;
  statusIcon.replaceChildren(icon(snapshot.running ? "check-circle" : "warning-circle"));
  $("status-text").textContent = snapshot.running ? "Gateway running" : "Gateway stopped";
  // The full URL is one click away (copy); the sidebar shows host:port.
  $("status-url").textContent = snapshot.url.replace(/^https?:\/\//, "");
  $("status-url").title = snapshot.url;
  const banner = $("banner");
  const problems = [];
  if (snapshot.error) problems.push(snapshot.error);
  if (!snapshot.config.models.default) problems.push("No default model yet. Add a provider under Providers, then pick a model under Models.");
  banner.replaceChildren(icon("warning-circle"), el("span", {}, problems.join("\n")));
  banner.classList.toggle("hidden", problems.length === 0);
}

// ---------- models ----------

function modelOptions() {
  const options = new Set(snapshot.advertised_models);
  for (const p of draft.providers) for (const m of p.models || []) options.add(`${p.id}/${m}`);
  return [...options];
}

function checkModelRef(value, { required = false } = {}) {
  if (!value) return required ? "Choose a default model, written provider_id/model_name." : null;
  if (!MODEL_REF.test(value)) return `"${value}" must be written provider_id/model_name.`;
  const provider = value.split("/")[0];
  if (!draft.providers.some((p) => p.id === provider)) return `No provider has the ID "${provider}". Add it under Providers or fix the prefix.`;
  return null;
}

function validateModels() {
  const results = [
    setError($("m-default"), checkModelRef(draft.models.default, { required: true })),
    ...["opus", "sonnet", "haiku"].map((tier) => setError($(`m-${tier}`), checkModelRef(draft.models[tier] || ""))),
  ];
  const bad = (draft.models.fallbacks || []).map((ref) => checkModelRef(ref)).find(Boolean);
  results.push(setError($("m-fallbacks"), bad ? `Fallback ${bad}` : null));
  return results.every(Boolean);
}

function renderModels() {
  const m = draft.models;
  $("m-default").value = m.default || "";
  $("m-opus").value = m.opus || "";
  $("m-sonnet").value = m.sonnet || "";
  $("m-haiku").value = m.haiku || "";
  $("m-fallbacks").value = (m.fallbacks || []).join("\n");
  $("model-options").replaceChildren(...modelOptions().map((value) => el("option", { value })));
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
  // Validate once the user leaves a field, not on every keystroke.
  for (const id of ["m-default", "m-opus", "m-sonnet", "m-haiku", "m-fallbacks"]) $(id).addEventListener("blur", validateModels);
}

// ---------- providers ----------

function checkProvider(p, index) {
  const errors = {};
  const id = (p.id || "").trim();
  if (!id) errors.id = "Enter an ID, for example nim.";
  else if (/[\s/]/.test(id)) errors.id = "The ID cannot contain spaces or slashes.";
  else if (draft.providers.some((other, i) => i !== index && other.id === id)) errors.id = `Another provider already uses "${id}".`;
  if (p.kind !== "antigravity" && !/^https?:\/\/\S+$/.test(p.base_url || "")) {
    errors.base_url = "Enter the full URL, starting with http:// or https://.";
  }
  return errors;
}

function renderProviderList() {
  const list = $("provider-list");
  if (draft.providers.length === 0) {
    list.replaceChildren(el("li", { class: "empty-state" }, "No providers yet. Pick a template below to add one."));
    return;
  }
  list.replaceChildren(...draft.providers.map((p, index) =>
    el("li", {},
      el("button", { type: "button", "aria-current": index === selected ? "true" : "false", onclick: () => { selected = index; renderProviders(); } },
        el("span", { class: "name" }, p.id || "(unnamed)"),
        el("span", { class: "kind" }, KIND_LABELS[p.kind] || "OpenAI")))));
}

function deleteButton(p) {
  return button("Delete provider", { variant: "danger", iconName: "trash", onclick: () => {
    if (!confirm(`Delete provider "${p.id}"? Models that use it will stop working until you change them.`)) return;
    draft.providers.splice(selected, 1);
    selected = draft.providers.length ? 0 : null;
    markDirty();
    renderProviders();
  } });
}

function renderEditor() {
  const box = $("provider-editor");
  const p = draft.providers[selected];
  if (!p) {
    box.replaceChildren(el("div", { class: "empty-state" },
      el("h2", {}, "No provider selected"),
      el("p", {}, "Select a provider on the left, or add one from a template.")));
    return;
  }
  if (p.kind === "antigravity") {
    renderAntigravity(p, box);
    return;
  }
  const preset = snapshot.presets.find((x) => x.id === p.id);
  const update = (key, transform = (v) => v) => (e) => {
    p[key] = transform(e.target.type === "checkbox" ? e.target.checked : e.target.value);
    markDirty();
    if (key === "id" || key === "kind") renderProviderList();
    if (key === "kind") renderEditor();
  };
  const input = (key, attrs = {}) => {
    const numeric = attrs.type === "number";
    const node = el("input", { ...attrs, oninput: update(key, numeric ? (v) => (v === "" ? null : Number(v)) : (v) => v) });
    node.value = p[key] ?? "";
    return node;
  };
  const check = (key, label, help) =>
    el("label", { class: "check" },
      Object.assign(el("input", { type: "checkbox", onchange: update(key) }), { checked: !!p[key] }),
      label, help ? el("span", { class: "hint-inline" }, help) : null);

  const result = el("div", { class: "result", role: "status" });
  const modelsArea = el("textarea", { rows: "4", class: "mono", oninput: update("models", (v) => v.split("\n").map((s) => s.trim()).filter(Boolean)) });
  modelsArea.value = (p.models || []).join("\n");
  const kind = el("select", { onchange: update("kind") },
    el("option", { value: "openai_chat" }, "OpenAI-compatible (/chat/completions)"),
    el("option", { value: "anthropic" }, "Anthropic-compatible (/v1/messages)"),
    el("option", { value: "antigravity" }, "Google Antigravity"));
  kind.value = p.kind || "openai_chat";

  const idInput = input("id", { class: "mono", autocomplete: "off" });
  const urlInput = input("base_url", { class: "mono", type: "url", autocomplete: "off" });
  const validate = () => {
    const errors = checkProvider(p, selected);
    setError(idInput, errors.id);
    setError(urlInput, errors.base_url);
  };
  idInput.addEventListener("blur", validate);
  urlInput.addEventListener("blur", validate);

  const fetchButton = button("Fetch models", { iconName: "list-magnifying-glass" });
  fetchButton.addEventListener("click", () => busy(fetchButton, async () => {
    message(result, "busy", "Loading the model list…");
    try {
      const ids = await invoke("fetch_models", { provider: p });
      const known = new Set(snapshot.advertised_models);
      ids.forEach((id) => known.add(`${p.id}/${id}`));
      snapshot.advertised_models = [...known];
      renderModels();
      message(result, "ok", `Found ${ids.length} models. They now appear as suggestions; add the ones you use above.`);
    } catch (err) {
      message(result, "bad", String(err));
    }
  }));
  const testButton = button("Test connection", { iconName: "flask" });
  testButton.addEventListener("click", () => busy(testButton, async () => {
    const model = (p.models || [])[0] || "";
    if (!model) {
      message(result, "bad", "Add at least one model above first; the first one is used for the test.");
      modelsArea.focus();
      return;
    }
    message(result, "busy", `Testing with ${model}…`);
    try {
      message(result, "ok", await invoke("test_provider", { provider: p, model }));
    } catch (err) {
      message(result, "bad", String(err));
    }
  }));

  box.replaceChildren(
    el("div", { class: "card-head" },
      el("h2", {}, p.id || "New provider"),
      preset && preset.key_url ? button("Get an API key", { iconName: "arrow-square-out", onclick: () => openUrl(preset.key_url) }) : null),
    el("div", { class: "grid" },
      field({ id: "p-id", label: "ID", control: idInput, help: "The prefix in model names, e.g. nim/…", required: true }),
      field({ id: "p-kind", label: "API type", control: kind })),
    field({ id: "p-url", label: "Base URL", control: urlInput, help: "Usually ends in /v1.", required: true }),
    el("div", { class: "grid" },
      field({ id: "p-key", label: "API key", control: input("api_key", { type: "password", autocomplete: "off" }), help: "Leave empty for local servers such as Ollama." }),
      field({ id: "p-key-env", label: "Or read the key from an environment variable", control: input("api_key_env", { class: "mono", autocomplete: "off" }), help: "For example NVIDIA_API_KEY." })),
    field({ id: "p-models", label: "Models", control: modelsArea, help: "One per line, without the provider prefix. The first is used for the connection test." }),
    el("div", { class: "actions start" }, fetchButton, testButton),
    result,
    el("div", { class: "section" },
      el("h3", {}, "Advanced"),
      el("div", { class: "grid" },
        field({ id: "p-cap", label: "Max output tokens", control: input("max_tokens_cap", { type: "number", min: "1", class: "mono" }),
          help: "Free tiers often limit output; Claude Code asks for far more by default. Empty means no limit." })),
      el("fieldset", {},
        el("legend", {}, "Request options"),
        check("reasoning_history", "Replay earlier reasoning", "needed by DeepSeek-style thinking models"),
        check("send_reasoning_effort", "Forward the reasoning effort"),
        check("stream_usage", "Ask for token usage while streaming"))),
    el("div", { class: "actions danger-zone" }, deleteButton(p), el("span", { class: "spacer" }), saveButton()),
  );
}

function renderProviders() {
  renderProviderList();
  renderEditor();
}

function megabytes(bytes) {
  return `${(bytes / 1048576).toFixed(0)} MB`;
}

/** Editor for the Antigravity provider: install, update, sign in, models. */
function renderAntigravity(p, box) {
  let latest = null;
  let license = null;
  const status = el("p", { class: "hint", role: "status" }, "Checking the ACP server…");
  const actions = el("div", { class: "actions start" });
  const auth = el("div", { class: "actions start" });
  const result = el("div", { class: "result", role: "status", id: "acp-result" });
  const modelsArea = el("textarea", { rows: "4", class: "mono", oninput: (e) => {
    p.models = e.target.value.split("\n").map((s) => s.trim()).filter(Boolean);
    markDirty();
  } });
  modelsArea.value = (p.models || []).join("\n");

  const run = (node, busyText, work) => busy(node, async () => {
    message(result, "busy", busyText);
    try {
      await work();
    } catch (err) {
      message(result, "bad", String(err));
    }
  });

  const fetchModels = async () => {
    const ids = await invoke("fetch_models", { provider: p });
    p.models = ids;
    modelsArea.value = ids.join("\n");
    if (!draft.models.default && ids.length) draft.models.default = `${p.id}/${ids[0]}`;
    markDirty();
    renderModels();
    message(result, "ok", `Signed in. ${ids.length} models available; save to keep them.`);
  };

  const draw = (view) => {
    if (view.latest_version) latest = view.latest_version;
    if (view.license_url) license = view.license_url;
    const installed = view.installed_version;
    const parts = [];
    if (installed) {
      parts.push(`Installed: v${installed}${view.running ? ", running" : ""}.`);
      if (latest && latest !== installed) parts.push(`Version ${latest} is available.`);
    } else {
      parts.push("Not installed yet. The server is downloaded from dl.google.com (several hundred MB).");
    }
    if (view.latest_error) parts.push(`Could not check for updates: ${view.latest_error}`);
    if (view.error) parts.push(`Last start failed: ${view.error}`);
    status.textContent = parts.join(" ");

    // Only the next step the user needs is the primary button.
    const updateAvailable = Boolean(installed && latest && latest !== installed);
    const label = !installed ? `Install${latest ? ` v${latest}` : ""}` : updateAvailable ? `Update to v${latest}` : "Reinstall";
    const installButton = button(label, { variant: !installed || updateAvailable ? "primary" : "", iconName: "download-simple" });
    installButton.addEventListener("click", () => run(installButton, "Downloading…", async () => {
      draw(await invoke("acp_install"));
      message(result, "ok", "Installed. Sign in below.");
    }));
    const buttons = [installButton];
    if (installed && !view.running) {
      const start = button("Start", { variant: updateAvailable ? "" : "primary", iconName: "arrow-clockwise" });
      start.addEventListener("click", () => run(start, "Starting the server; this can take about 20 seconds…", async () => {
        draw(await invoke("acp_status", { start: true, checkLatest: false }));
        result.replaceChildren();
      }));
      buttons.push(start);
    }
    if (license) buttons.push(button("Terms", { iconName: "arrow-square-out", onclick: () => openUrl(license) }));
    actions.replaceChildren(...buttons);

    if (!view.running) {
      auth.replaceChildren(el("p", { class: "hint" }, installed ? "Start the server to see the sign-in options." : "Install the server first."));
    } else {
      auth.replaceChildren(...view.auth_methods.map((method) => {
        const node = button(method.name || method.id, { iconName: "sign-in" });
        node.addEventListener("click", () => run(node, "Waiting for sign-in to finish; a browser window may open…", async () => {
          draw(await invoke("acp_authenticate", { methodId: method.id }));
          await fetchModels();
        }));
        return node;
      }));
    }
  };

  const fetchButton = button("Fetch models", { iconName: "list-magnifying-glass" });
  fetchButton.addEventListener("click", () => run(fetchButton, "Loading models…", fetchModels));
  const testButton = button("Test connection", { iconName: "flask" });
  testButton.addEventListener("click", () => {
    const model = (p.models || [])[0] || "";
    run(testButton, `Testing with ${model || "the default model"}…`, async () => {
      message(result, "ok", await invoke("test_provider", { provider: p, model: model || "default" }));
    });
  });

  box.replaceChildren(
    el("h2", {}, "Google Antigravity"),
    el("p", { class: "hint" },
      "Runs through Google's ACP server. Claude Code and Codex keep their own tools; the agent only acts as the model. " +
      "Review Google's terms before relying on it."),
    el("div", { class: "section" }, el("h3", {}, "Server"), status, actions),
    el("div", { class: "section" }, el("h3", {}, "Sign in"), auth),
    el("div", { class: "section" },
      field({ id: "acp-models", label: "Models", control: modelsArea, help: "Fetched after sign-in. One per line." }),
      el("div", { class: "actions start" }, fetchButton, testButton),
      result),
    el("div", { class: "actions danger-zone" }, deleteButton(p), el("span", { class: "spacer" }), saveButton()),
  );
  invoke("acp_status", { start: false, checkLatest: true }).then(draw, (err) => (status.textContent = String(err)));
}

if (window.__TAURI__) {
  window.__TAURI__.event.listen("acp-progress", ({ payload }) => {
    const node = $("acp-result");
    if (!node) return;
    message(node, "busy", payload.total
      ? `Downloading… ${megabytes(payload.downloaded)} of ${megabytes(payload.total)}`
      : `Downloading… ${megabytes(payload.downloaded)}`);
  });
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
  $("p-id")?.focus();
}

function openUrl(url) {
  if (window.__TAURI__) invoke("plugin:opener|open_url", { url });
  else window.open(url, "_blank", "noopener");
}

// ---------- usage ----------

function renderUsage() {
  const c = snapshot.config;
  const url = snapshot.url;
  $("claude-env").textContent =
    `# macOS / Linux\nexport ANTHROPIC_BASE_URL=${url}\nexport ANTHROPIC_AUTH_TOKEN=${c.server.auth_token}\nexport CLAUDE_CODE_AUTO_MODE_SERVER=0\nunset ANTHROPIC_API_KEY\nclaude\n\n` +
    `# Windows PowerShell\n$env:ANTHROPIC_BASE_URL="${url}"\n$env:ANTHROPIC_AUTH_TOKEN="${c.server.auth_token}"\n$env:CLAUDE_CODE_AUTO_MODE_SERVER="0"\nclaude`;
  $("codex-env").textContent =
    `model_provider = "ferry"\nmodel = ${JSON.stringify(c.models.default || "provider/model")}\n\n` +
    `[model_providers.ferry]\nname = "Ferry"\nbase_url = "${url}/v1"\nwire_api = "responses"\nenv_key = "FERRY_API_KEY"\n\n` +
    `# and in your shell: export FERRY_API_KEY=${c.server.auth_token}`;
}

// ---------- requests ----------

async function refreshRequests() {
  if (page !== "requests") return;
  const rows = await invoke("recent_requests");
  $("req-count").textContent = rows.length ? `${rows.length} shown.` : "";
  if (!rows.length) {
    $("req-list").replaceChildren(el("li", { class: "empty-state request-empty" },
      "No requests yet. Run ferry claude or ferry codex, and each request shows up here."));
    return;
  }
  $("req-list").replaceChildren(...rows.map((r) => {
    const ok = r.outcome.startsWith("ok");
    // "ok" alone needs no detail line; anything else (errors, local, resumed) does.
    const detail = r.outcome === "ok" ? null : r.outcome;
    return el("li", { class: `request ${ok ? "ok" : "bad"}` },
      el("span", { class: "request-status" }, icon(ok ? "check-circle" : "warning-circle"),
        el("span", { class: "sr-only" }, ok ? "Succeeded" : "Failed")),
      el("div", { class: "request-body" },
        el("p", { class: "request-route" },
          el("span", { class: "mono" }, r.requested_model || "(no model)"),
          el("span", { class: "arrow", "aria-hidden": "true" }, " → "),
          el("span", { class: "sr-only" }, " routed to "),
          el("span", { class: "mono" }, r.routed_model || "(not routed)")),
        el("p", { class: "request-meta" },
          `${new Date(r.started_at_ms).toLocaleTimeString()} · ${r.api === "messages" ? "Claude Code" : "Codex"} · ` +
          `${(r.duration_ms / 1000).toFixed(1)} s · ${r.input_tokens} in / ${r.output_tokens} out tokens`),
        detail ? el("p", { class: "request-detail" }, detail) : null));
  }));
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
  const o = draft.optimizations || {};
  $("o-titles").checked = o.local_session_titles !== false;
  $("o-quota").checked = o.local_quota_probe !== false;
}

function validateSettings() {
  const port = Number(draft.server.port);
  return setError($("s-port"), Number.isInteger(port) && port >= 1 && port <= 65535 ? null : "Enter a port from 1 to 65535.");
}

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  $("theme").value = theme;
}

function bindSettings() {
  const num = (key) => (e) => { draft.server[key] = Number(e.target.value); markDirty(); };
  $("s-host").addEventListener("input", (e) => { draft.server.host = e.target.value.trim(); markDirty(); });
  $("s-port").addEventListener("input", num("port"));
  $("s-port").addEventListener("blur", validateSettings);
  $("s-timeout").addEventListener("input", num("connect_timeout_secs"));
  $("s-retries").addEventListener("input", num("max_retries"));
  $("s-token").addEventListener("input", (e) => { draft.server.auth_token = e.target.value.trim(); markDirty(); });
  const toggle = (key) => (e) => {
    draft.optimizations = { local_session_titles: true, local_quota_probe: true, ...draft.optimizations, [key]: e.target.checked };
    markDirty();
  };
  $("o-titles").addEventListener("change", toggle("local_session_titles"));
  $("o-quota").addEventListener("change", toggle("local_quota_probe"));
  $("regen-token").addEventListener("click", (e) => busy(e.currentTarget, async () => {
    draft.server.auth_token = await invoke("new_token");
    $("s-token").value = draft.server.auth_token;
    markDirty();
  }));
  $("restart").addEventListener("click", (e) => busy(e.currentTarget, async () => {
    apply(await invoke("restart_gateway"), false);
    toast(snapshot.running ? "Gateway restarted" : "The gateway failed to start", snapshot.running ? "ok" : "bad");
  }));

  // The theme is a per-machine preference, kept out of the shared config file.
  let theme = "dark";
  try { theme = localStorage.getItem("ferry.theme") || "dark"; } catch { /* storage unavailable */ }
  applyTheme(theme);
  $("theme").addEventListener("change", (e) => {
    applyTheme(e.target.value);
    try { localStorage.setItem("ferry.theme", e.target.value); } catch { /* storage unavailable */ }
  });
}

// ---------- lifecycle ----------

function apply(next, resetDraft = true) {
  snapshot = next;
  if (resetDraft || !draft) {
    draft = structuredClone(next.config);
    if (selected != null && selected >= draft.providers.length) selected = null;
    if (selected == null && draft.providers.length) selected = 0;
    clearDirty();
  }
  renderStatus();
  renderModels();
  renderProviders();
  renderUsage();
  renderSettings();
}

/** Check everything before saving; jump to the first problem. */
function validateAll() {
  const badProvider = draft.providers.findIndex((p, i) => Object.keys(checkProvider(p, i)).length);
  if (badProvider >= 0) {
    selected = badProvider;
    renderProviders();
    location.hash = "providers";
    const errors = checkProvider(draft.providers[badProvider], badProvider);
    setError($("p-id"), errors.id);
    setError($("p-url"), errors.base_url);
    ($("p-id")?.getAttribute("aria-invalid") === "true" ? $("p-id") : $("p-url"))?.focus();
    return false;
  }
  if (!validateModels()) {
    location.hash = "models";
    document.querySelector('#page-models [aria-invalid="true"]')?.focus();
    return false;
  }
  if (!validateSettings()) {
    location.hash = "settings";
    $("s-port").focus();
    return false;
  }
  return true;
}

async function save() {
  if (!validateAll()) {
    toast("Fix the highlighted field before saving", "bad");
    return;
  }
  try {
    apply(await invoke("save_config", { config: draft }));
    toast("Saved. Changes apply immediately.");
  } catch (err) {
    toast(`Save failed: ${err}`, "bad", 8000);
  }
}

async function main() {
  decorateIcons();
  bindNavigation();
  bindModels();
  bindSettings();
  document.addEventListener("click", (e) => {
    if (e.target.closest("[data-save]")) save();
    const copyTarget = e.target.closest("[data-copy]");
    if (copyTarget) copy($(copyTarget.dataset.copy).textContent);
  });
  $("copy-url").addEventListener("click", () => copy(snapshot.url));
  $("add-provider").addEventListener("click", addProvider);
  $("install-cli").addEventListener("click", (e) => busy(e.currentTarget, async () => {
    try {
      toast(await invoke("install_cli"), "ok", 9000);
    } catch (err) {
      toast(`Install failed: ${err}`, "bad", 9000);
    }
  }));
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
  document.body.prepend(el("div", { class: "banner", role: "alert" }, `The settings UI failed to start: ${err}`));
});
