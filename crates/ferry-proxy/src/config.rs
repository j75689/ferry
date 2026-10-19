//! User configuration: one TOML file shared by the desktop app and the CLI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 8787;
const CONFIG_ENV: &str = "FERRY_CONFIG";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub models: ModelConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub optimizations: Optimizations,
    #[serde(default)]
    pub web_search: WebSearchConfig,
}

/// Where Ferry searches when a client's hosted web search tool is used
/// (Claude Code's WebSearch, Codex's `web_search`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebSearchConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub backend: SearchBackend,
    /// Endpoint override; required for SearXNG (its base URL).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// API key for Brave Search.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: SearchBackend::default(),
            url: String::new(),
            api_key: String::new(),
            api_key_env: None,
            max_results: default_max_results(),
        }
    }
}

impl WebSearchConfig {
    pub fn resolved_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchBackend {
    /// DuckDuckGo's HTML results page; no key.
    #[default]
    Duckduckgo,
    /// Brave Search API; needs `api_key`.
    Brave,
    /// A SearXNG instance with the JSON format enabled; needs `url`.
    Searxng,
}

fn default_max_results() -> usize {
    8
}

/// Requests Ferry answers itself instead of spending a provider call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Optimizations {
    /// Claude Code asks for a session title on every turn.
    #[serde(default = "default_true")]
    pub local_session_titles: bool,
    /// Older Claude Code versions probe the quota with a one-token request.
    #[serde(default = "default_true")]
    pub local_quota_probe: bool,
}

impl Default for Optimizations {
    fn default() -> Self {
        Self {
            local_session_titles: true,
            local_quota_probe: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Clients must send this as `x-api-key` or `Authorization: Bearer`.
    /// An empty token disables authentication.
    #[serde(default)]
    pub auth_token: String,
    /// Seconds to wait for the upstream to start responding.
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Attempts per candidate for retryable upstream failures (429 / 5xx).
    #[serde(default = "default_retries")]
    pub max_retries: u32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth_token: String::new(),
            connect_timeout_secs: default_connect_timeout(),
            max_retries: default_retries(),
        }
    }
}

/// Model references are written `provider_id/model_name`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelConfig {
    /// Used when a request names no mapped tier and no explicit provider.
    #[serde(default)]
    pub default: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opus: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sonnet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haiku: Option<String>,
    /// Tried in order after the primary model fails before streaming starts.
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// OpenAI-compatible `POST {base_url}/chat/completions`.
    #[default]
    OpenaiChat,
    /// Anthropic-compatible `POST {base_url}/v1/messages`.
    Anthropic,
    /// Google Antigravity through its official ACP server; no URL or key.
    Antigravity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub kind: ProviderKind,
    #[serde(default)]
    pub base_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Read the key from this environment variable when `api_key` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Clamp the client's max output tokens; many free tiers reject large values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens_cap: Option<u32>,
    /// Forward client thinking/reasoning effort as `reasoning_effort`.
    #[serde(default)]
    pub send_reasoning_effort: bool,
    /// Replay earlier reasoning as `reasoning_content` on assistant turns
    /// (DeepSeek-style thinking models require this around tool calls).
    #[serde(default)]
    pub reasoning_history: bool,
    /// Ask for `stream_options.include_usage`; disable for servers that reject it.
    #[serde(default = "default_true")]
    pub stream_usage: bool,
    /// Models advertised to clients from `/v1/models` (in addition to routed ones).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Context window of this provider's models, in tokens, as told to Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

impl ProviderConfig {
    pub fn resolved_api_key(&self) -> String {
        if !self.api_key.is_empty() {
            return self.api_key.clone();
        }
        self.api_key_env
            .as_deref()
            .and_then(|name| std::env::var(name).ok())
            .unwrap_or_default()
    }

    pub fn trimmed_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_connect_timeout() -> u64 {
    120
}
fn default_retries() -> u32 {
    2
}
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                auth_token: generate_token(),
                ..ServerConfig::default()
            },
            models: ModelConfig::default(),
            providers: Vec::new(),
            optimizations: Optimizations::default(),
            web_search: WebSearchConfig::default(),
        }
    }
}

pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("ferry-{hex}")
}

impl Config {
    pub fn default_path() -> PathBuf {
        if let Ok(path) = std::env::var(CONFIG_ENV) {
            if !path.trim().is_empty() {
                return PathBuf::from(path);
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ferry")
            .join("config.toml")
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).context("invalid config TOML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("in {}", path.display()))
    }

    /// Load the file, creating it with a fresh auth token on first use.
    pub fn load_or_init(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let config = Config::default();
        config.save(path)?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml()?)?;
        restrict_permissions(&tmp);
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.web_search.enabled
            && self.web_search.backend == SearchBackend::Searxng
            && self.web_search.url.trim().is_empty()
        {
            bail!("web_search: the searxng backend needs url");
        }
        let models = &self.models;
        let tiers = [&models.opus, &models.sonnet, &models.haiku];
        let references = std::iter::once(&models.default)
            .chain(tiers.into_iter().flatten())
            .chain(models.fallbacks.iter())
            .filter(|r| !r.trim().is_empty());
        for reference in references {
            if crate::router::split_reference(reference).is_none() {
                bail!("model {reference:?} must be written provider_id/model_name");
            }
        }
        let mut seen = std::collections::HashSet::new();
        for provider in &self.providers {
            let id = provider.id.trim();
            if id.is_empty() || id.contains('/') || id.contains(char::is_whitespace) {
                bail!(
                    "provider id {:?} must be non-empty without '/' or spaces",
                    provider.id
                );
            }
            if !seen.insert(id) {
                bail!("duplicate provider id {id:?}");
            }
            if provider.kind != ProviderKind::Antigravity
                && !(provider.base_url.starts_with("http://")
                    || provider.base_url.starts_with("https://"))
            {
                bail!("provider {id:?} base_url must start with http:// or https://");
            }
        }
        Ok(())
    }

    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.iter().find(|p| p.id == id)
    }

    pub fn local_url(&self) -> String {
        let host = match self.server.host.as_str() {
            "0.0.0.0" | "" => "127.0.0.1",
            "::" => "[::1]",
            other => other,
        };
        format!("http://{host}:{}", self.server.port)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_minimal_file() {
        let config = Config::from_toml(
            r#"
            [models]
            default = "nim/meta/llama-3.3-70b-instruct"

            [[providers]]
            id = "nim"
            base_url = "https://integrate.api.nvidia.com/v1"
            api_key = "k"
            "#,
        )
        .unwrap();
        assert_eq!(config.server.port, DEFAULT_PORT);
        assert_eq!(config.providers[0].kind, ProviderKind::OpenaiChat);
        assert!(config.providers[0].stream_usage);
        let again = Config::from_toml(&config.to_toml().unwrap()).unwrap();
        assert_eq!(config, again);
    }

    #[test]
    fn rejects_model_without_provider() {
        let err = Config::from_toml("[models]\nfallbacks = [\"mock-1\"]").unwrap_err();
        assert!(format!("{err:#}").contains("provider_id/model_name"));
    }

    #[test]
    fn rejects_bad_provider_ids() {
        let err = Config::from_toml(
            r#"
            [[providers]]
            id = "a/b"
            base_url = "https://x"
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("must be non-empty"));
    }
}
