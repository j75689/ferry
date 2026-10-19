//! Model metadata for Codex.
//!
//! Codex looks up every model in its built-in catalog and warns ("Model
//! metadata not found") when the slug is unknown, which is the case for every
//! `provider/model` Ferry routes. Codex reads a replacement catalog from the
//! `model_catalog_json` setting; this builds one for Ferry's models.

use serde_json::{json, Value};

use crate::config::Config;
use crate::router;

/// Context window assumed when a provider does not set one.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

const BASE_INSTRUCTIONS: &str = "You are Codex, a coding agent running in the user's terminal. \
Help the user understand, change, test, and review code in their workspace. Use the available \
tools when needed, follow the user's instructions, and report concisely what you did and how \
you verified it.";

/// The catalog for every model Ferry advertises, the default first.
pub fn catalog(config: &Config) -> Value {
    let models: Vec<Value> = router::advertised_models(config)
        .iter()
        .enumerate()
        .map(|(priority, slug)| entry(config, slug, priority))
        .collect();
    json!({ "models": models })
}

fn entry(config: &Config, slug: &str, priority: usize) -> Value {
    let provider = router::split_reference(slug)
        .and_then(|(id, _)| config.providers.iter().find(|p| p.id == id));
    let window = provider
        .and_then(|p| p.context_window)
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let reasoning = provider.is_some_and(|p| p.send_reasoning_effort);
    let levels: Vec<Value> = if reasoning {
        [
            ("low", "Fast responses with lighter reasoning"),
            ("medium", "Balances speed and reasoning depth"),
            ("high", "Greater reasoning depth for complex problems"),
        ]
        .iter()
        .map(|(effort, description)| json!({"effort": effort, "description": description}))
        .collect()
    } else {
        Vec::new()
    };
    let mut entry = json!({
        "slug": slug,
        "display_name": slug,
        "description": "Served through Ferry",
        "supported_reasoning_levels": levels,
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": priority,
        "base_instructions": BASE_INSTRUCTIONS,
        "support_verbosity": false,
        "truncation_policy": {"mode": "tokens", "limit": 10000},
        "experimental_supported_tools": [],
        "apply_patch_tool_type": "freeform",
        "supports_parallel_tool_calls": true,
        "supports_reasoning_summaries": reasoning,
        "context_window": window,
        "max_context_window": window,
        "effective_context_window_percent": 95,
        "input_modalities": ["text", "image"],
    });
    if reasoning {
        entry["default_reasoning_level"] = json!("medium");
    }
    entry
}

/// Whether a `codex --version` line names a version known to read the
/// catalog format above (0.163.0 or later).
pub fn supported_version(version_line: &str) -> bool {
    let Some(version) = version_line.split_whitespace().last() else {
        return false;
    };
    let parts: Vec<u64> = version
        .split(['.', '-'])
        .take(3)
        .map_while(|p| p.parse().ok())
        .collect();
    matches!(parts.as_slice(), [major, minor, _] if (*major, *minor) >= (0, 163))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_advertised_models_with_metadata() {
        let config = Config::from_toml(
            r#"
            [models]
            default = "nim/llama"
            haiku = "ds/chat"

            [[providers]]
            id = "nim"
            base_url = "http://x/v1"
            context_window = 32000

            [[providers]]
            id = "ds"
            base_url = "http://y/v1"
            send_reasoning_effort = true
            "#,
        )
        .unwrap();
        let models = catalog(&config)["models"].as_array().unwrap().clone();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "nim/llama");
        assert_eq!(models[0]["priority"], 0);
        assert_eq!(models[0]["context_window"], 32000);
        assert_eq!(models[0]["supported_reasoning_levels"], json!([]));
        assert!(models[0].get("default_reasoning_level").is_none());
        assert_eq!(models[1]["context_window"], DEFAULT_CONTEXT_WINDOW);
        assert_eq!(models[1]["default_reasoning_level"], "medium");
        for field in [
            "display_name",
            "shell_type",
            "visibility",
            "supported_in_api",
            "base_instructions",
            "support_verbosity",
            "truncation_policy",
            "experimental_supported_tools",
        ] {
            assert!(models[1].get(field).is_some(), "{field}");
        }
    }

    #[test]
    fn checks_the_codex_version() {
        assert!(supported_version("codex-cli 0.163.0"));
        assert!(supported_version("codex-cli 0.170.2-alpha.1"));
        assert!(supported_version("codex-cli 1.0.0"));
        assert!(!supported_version("codex-cli 0.162.9"));
        assert!(!supported_version("garbage"));
    }
}
