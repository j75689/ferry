//! Starting points for common providers. Users can edit everything afterwards.

use serde::Serialize;

use crate::config::{ProviderConfig, ProviderKind};

#[derive(Debug, Clone, Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ProviderKind,
    pub base_url: &'static str,
    /// Where users create an API key; empty for local servers.
    pub key_url: &'static str,
    pub example_model: &'static str,
    pub max_tokens_cap: Option<u32>,
    pub reasoning_history: bool,
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "nim",
        name: "NVIDIA NIM",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://integrate.api.nvidia.com/v1",
        key_url: "https://build.nvidia.com/settings/api-keys",
        example_model: "meta/llama-3.3-70b-instruct",
        max_tokens_cap: Some(16384),
        reasoning_history: false,
    },
    Preset {
        id: "openrouter",
        name: "OpenRouter",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://openrouter.ai/api/v1",
        key_url: "https://openrouter.ai/settings/keys",
        example_model: "deepseek/deepseek-chat-v3.1:free",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "deepseek",
        name: "DeepSeek",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://api.deepseek.com",
        key_url: "https://platform.deepseek.com/api_keys",
        example_model: "deepseek-chat",
        max_tokens_cap: Some(8192),
        reasoning_history: true,
    },
    Preset {
        id: "groq",
        name: "Groq",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://api.groq.com/openai/v1",
        key_url: "https://console.groq.com/keys",
        example_model: "openai/gpt-oss-120b",
        max_tokens_cap: Some(32768),
        reasoning_history: false,
    },
    Preset {
        id: "gemini",
        name: "Google Gemini (OpenAI compatible)",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_url: "https://aistudio.google.com/apikey",
        example_model: "gemini-2.5-flash",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "mistral",
        name: "Mistral",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://api.mistral.ai/v1",
        key_url: "https://console.mistral.ai/api-keys",
        example_model: "devstral-medium-latest",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "cerebras",
        name: "Cerebras",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://api.cerebras.ai/v1",
        key_url: "https://cloud.cerebras.ai",
        example_model: "qwen-3-coder-480b",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "openai",
        name: "OpenAI",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://api.openai.com/v1",
        key_url: "https://platform.openai.com/api-keys",
        example_model: "gpt-4.1",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "anthropic",
        name: "Anthropic",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com",
        key_url: "https://console.anthropic.com/settings/keys",
        example_model: "claude-sonnet-4-5",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "antigravity",
        name: "Google Antigravity (subscription, via ACP)",
        kind: ProviderKind::Antigravity,
        base_url: "",
        key_url: "",
        example_model: "",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "ollama",
        name: "Ollama (local)",
        kind: ProviderKind::OpenaiChat,
        base_url: "http://127.0.0.1:11434/v1",
        key_url: "",
        example_model: "qwen3-coder",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "lmstudio",
        name: "LM Studio (local)",
        kind: ProviderKind::OpenaiChat,
        base_url: "http://127.0.0.1:1234/v1",
        key_url: "",
        example_model: "",
        max_tokens_cap: None,
        reasoning_history: false,
    },
    Preset {
        id: "custom",
        name: "Custom OpenAI-compatible",
        kind: ProviderKind::OpenaiChat,
        base_url: "https://",
        key_url: "",
        example_model: "",
        max_tokens_cap: None,
        reasoning_history: false,
    },
];

impl Preset {
    pub fn to_provider(&self) -> ProviderConfig {
        ProviderConfig {
            id: self.id.to_string(),
            kind: self.kind,
            base_url: self.base_url.to_string(),
            api_key: String::new(),
            api_key_env: None,
            headers: Default::default(),
            max_tokens_cap: self.max_tokens_cap,
            send_reasoning_effort: false,
            reasoning_history: self.reasoning_history,
            stream_usage: true,
            models: if self.example_model.is_empty() {
                Vec::new()
            } else {
                vec![self.example_model.to_string()]
            },
            context_window: None,
        }
    }
}
