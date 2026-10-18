//! Map the model a client asked for onto configured `provider/model` targets.

use crate::config::{Config, ProviderConfig};

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub provider: ProviderConfig,
    pub model: String,
}

impl Target {
    pub fn reference(&self) -> String {
        format!("{}/{}", self.provider.id, self.model)
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RouteError {
    #[error("no model configured: set models.default (requested {0:?})")]
    NoDefault(String),
    #[error("model reference {0:?} must look like provider_id/model")]
    BadReference(String),
    #[error("unknown provider {0:?} in model reference")]
    UnknownProvider(String),
}

/// Split `provider/model`; the model part may itself contain slashes.
pub fn split_reference(reference: &str) -> Option<(&str, &str)> {
    let (provider, model) = reference.trim().split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

fn target_for(config: &Config, reference: &str) -> Result<Target, RouteError> {
    let (provider_id, model) =
        split_reference(reference).ok_or_else(|| RouteError::BadReference(reference.into()))?;
    let provider = config
        .provider(provider_id)
        .ok_or_else(|| RouteError::UnknownProvider(provider_id.into()))?;
    Ok(Target {
        provider: provider.clone(),
        model: model.to_string(),
    })
}

/// The primary target followed by deduplicated fallbacks.
pub fn resolve(config: &Config, requested: &str) -> Result<Vec<Target>, RouteError> {
    let primary_ref = primary_reference(config, requested)?;
    let mut targets = vec![target_for(config, &primary_ref)?];
    for fallback in &config.models.fallbacks {
        // A broken fallback entry should not take down the primary route.
        let Ok(target) = target_for(config, fallback) else {
            tracing::warn!(fallback, "skipping invalid fallback model reference");
            continue;
        };
        if !targets.iter().any(|t| t.reference() == target.reference()) {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn primary_reference(config: &Config, requested: &str) -> Result<String, RouteError> {
    // Explicit `provider/model` from a model picker wins.
    if let Some((provider, _)) = split_reference(requested) {
        if config.provider(provider).is_some() {
            return Ok(requested.trim().to_string());
        }
    }
    let lowered = requested.to_ascii_lowercase();
    let tiers = [
        ("opus", &config.models.opus),
        ("sonnet", &config.models.sonnet),
        ("haiku", &config.models.haiku),
    ];
    for (tier, mapped) in tiers {
        if let Some(mapped) = mapped.as_deref().filter(|m| !m.trim().is_empty()) {
            if lowered.contains(tier) {
                return Ok(mapped.to_string());
            }
        }
    }
    if config.models.default.trim().is_empty() {
        return Err(RouteError::NoDefault(requested.into()));
    }
    Ok(config.models.default.clone())
}

/// Every model reference worth advertising to clients, in a stable order.
pub fn advertised_models(config: &Config) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |reference: &str| {
        let reference = reference.trim();
        if split_reference(reference).is_some() && !out.iter().any(|m| m == reference) {
            out.push(reference.to_string());
        }
    };
    push(&config.models.default);
    for reference in [
        &config.models.opus,
        &config.models.sonnet,
        &config.models.haiku,
    ]
    .into_iter()
    .flatten()
    {
        push(reference);
    }
    for reference in &config.models.fallbacks {
        push(reference);
    }
    for provider in &config.providers {
        for model in &provider.models {
            push(&format!("{}/{}", provider.id, model));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_toml(
            r#"
            [models]
            default = "nim/meta/llama"
            haiku = "groq/fast"
            fallbacks = ["groq/fast", "nim/meta/llama", "missing/x"]

            [[providers]]
            id = "nim"
            base_url = "https://nim"

            [[providers]]
            id = "groq"
            base_url = "https://groq"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn tiers_and_default() {
        let c = config();
        let refs = |m: &str| {
            resolve(&c, m)
                .unwrap()
                .iter()
                .map(Target::reference)
                .collect::<Vec<_>>()
        };
        assert_eq!(refs("claude-haiku-4-5"), ["groq/fast", "nim/meta/llama"]);
        assert_eq!(refs("claude-sonnet-4-5"), ["nim/meta/llama", "groq/fast"]);
        assert_eq!(
            refs("groq/other/model"),
            ["groq/other/model", "groq/fast", "nim/meta/llama"]
        );
    }

    #[test]
    fn unknown_prefix_is_not_a_provider() {
        let c = config();
        let first = &resolve(&c, "anthropic/claude").unwrap()[0];
        assert_eq!(first.reference(), "nim/meta/llama");
    }

    #[test]
    fn missing_default_is_an_error() {
        let c = Config::from_toml("").unwrap();
        assert!(matches!(resolve(&c, "x"), Err(RouteError::NoDefault(_))));
    }
}
