//! `ferry`: run native Claude Code or Codex through the Ferry gateway.
//!
//! If the desktop app (or `ferry serve`) is already listening on the configured
//! port it is reused; otherwise a private gateway is started on a free port
//! for the lifetime of the launched tool.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ferry_proxy::{AppState, Config};

const USAGE: &str = "\
ferry - Ferry gateway

USAGE:
    ferry claude [ARGS...]     Run Claude Code through Ferry
    ferry codex [ARGS...]      Run Codex through Ferry
    ferry serve                Run the gateway in the foreground
    ferry env [claude|codex]   Print settings for manual setup
    ferry models               List models Ferry advertises
    ferry config               Print the config file path

Config file: $FERRY_CONFIG or the platform config dir (ferry/config.toml).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest.to_vec()),
        None => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    let filter = if command == "serve" { "info" } else { "warn" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG")
                .unwrap_or_else(|_| filter.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = runtime.block_on(async {
        match command {
            "claude" => launch(Tool::Claude, rest).await,
            "codex" => launch(Tool::Codex, rest).await,
            "serve" => serve().await.map(|_| 0),
            "env" => print_env(rest.first().map(String::as_str)).await.map(|_| 0),
            "models" => print_models().map(|_| 0),
            "config" => {
                println!("{}", Config::default_path().display());
                Ok(0)
            }
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                Ok(0)
            }
            other => {
                eprintln!("unknown command {other:?}\n\n{USAGE}");
                Ok(2)
            }
        }
    });
    match result {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("ferry: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn load_config() -> Result<(Config, std::path::PathBuf)> {
    let path = Config::default_path();
    let config = Config::load_or_init(&path)?;
    Ok((config, path))
}

async fn serve() -> Result<()> {
    let (config, path) = load_config()?;
    let state = AppState::new(config.clone(), Some(path.clone()));
    let server = ferry_proxy::spawn(state, &config.server.host, config.server.port)
        .await
        .with_context(|| {
            format!(
                "cannot listen on {}:{}",
                config.server.host, config.server.port
            )
        })?;
    eprintln!(
        "Ferry gateway on {} (config: {})",
        server.url(),
        path.display()
    );
    if config.models.default.is_empty() {
        eprintln!(
            "warning: models.default is empty; add a provider in the desktop app or config file"
        );
    }
    tokio::signal::ctrl_c().await?;
    server.stop().await;
    Ok(())
}

/// Is an Ferry gateway already answering at `url`?
async fn existing_gateway(url: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(800))
        .no_proxy()
        .build();
    let Ok(client) = client else { return false };
    match client.get(format!("{url}/health")).send().await {
        Ok(response) => response
            .json::<serde_json::Value>()
            .await
            .is_ok_and(|body| body["service"] == "ferry"),
        Err(_) => false,
    }
}

/// Returns the gateway URL and, when we started it, the server handle.
async fn ensure_gateway(
    config: &Config,
    path: &std::path::Path,
) -> Result<(String, Option<ferry_proxy::RunningServer>)> {
    let shared = config.local_url();
    if existing_gateway(&shared).await {
        return Ok((shared, None));
    }
    let state = AppState::new(config.clone(), Some(path.to_path_buf()));
    let server = ferry_proxy::spawn(state, "127.0.0.1", 0)
        .await
        .context("cannot start gateway")?;
    Ok((server.url(), Some(server)))
}

#[derive(Clone, Copy)]
enum Tool {
    Claude,
    Codex,
}

impl Tool {
    fn binary(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            Tool::Claude => "npm install -g @anthropic-ai/claude-code",
            Tool::Codex => "npm install -g @openai/codex",
        }
    }
}

struct Plan {
    args: Vec<String>,
    set: HashMap<String, String>,
    remove_prefixes: Vec<&'static str>,
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serializes")
}

fn plan(tool: Tool, url: &str, config: &Config, user_args: Vec<String>) -> Plan {
    let token = config.server.auth_token.clone();
    let mut set = HashMap::new();
    match tool {
        Tool::Claude => {
            set.insert("ANTHROPIC_BASE_URL".into(), url.to_string());
            set.insert(
                "ANTHROPIC_AUTH_TOKEN".into(),
                if token.is_empty() {
                    "ferry".into()
                } else {
                    token
                },
            );
            // Let Claude Code's /model picker list Ferry's models.
            set.insert(
                "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY".into(),
                "1".into(),
            );
            Plan {
                args: user_args,
                set,
                remove_prefixes: vec!["ANTHROPIC_"],
            }
        }
        Tool::Codex => {
            set.insert(
                "FERRY_API_KEY".into(),
                if token.is_empty() {
                    "ferry".into()
                } else {
                    token
                },
            );
            let mut args = Vec::new();
            let mut overrides = vec![
                ("model_provider", toml_string("ferry")),
                ("model_providers.ferry.name", toml_string("Ferry")),
                (
                    "model_providers.ferry.base_url",
                    toml_string(&format!("{url}/v1")),
                ),
                ("model_providers.ferry.wire_api", toml_string("responses")),
                (
                    "model_providers.ferry.env_key",
                    toml_string("FERRY_API_KEY"),
                ),
            ];
            if !config.models.default.is_empty() {
                overrides.push(("model", toml_string(&config.models.default)));
            }
            for (key, value) in overrides {
                args.push("-c".to_string());
                args.push(format!("{key}={value}"));
            }
            args.extend(user_args);
            Plan {
                args,
                set,
                remove_prefixes: vec![],
            }
        }
    }
}

fn child_env(plan: &Plan) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| !plan.remove_prefixes.iter().any(|p| key.starts_with(p)))
        .filter(|(key, _)| !plan.set.contains_key(key))
        .collect();
    env.extend(plan.set.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Keep local gateway traffic away from HTTP(S)_PROXY.
    for key in ["NO_PROXY", "no_proxy"] {
        let mut value = std::env::var(key).unwrap_or_default();
        for host in ["127.0.0.1", "localhost", "::1"] {
            if !value.split(',').any(|h| h.trim() == host) {
                if !value.is_empty() {
                    value.push(',');
                }
                value.push_str(host);
            }
        }
        env.retain(|(k, _)| k != key);
        env.push((key.to_string(), value));
    }
    env
}

async fn launch(tool: Tool, user_args: Vec<String>) -> Result<i32> {
    let binary = which::which(tool.binary()).with_context(|| {
        format!(
            "`{}` was not found on PATH. Install it with: {}",
            tool.binary(),
            tool.install_hint()
        )
    })?;
    let (config, path) = load_config()?;
    if config.models.default.is_empty() {
        bail!(
            "no default model configured. Open the Ferry app or edit {} and set models.default",
            path.display()
        );
    }
    let (url, owned) = ensure_gateway(&config, &path).await?;
    let plan = plan(tool, &url, &config, user_args);

    // The child owns the terminal; keep Ctrl-C from killing our gateway.
    tokio::spawn(async {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
        }
    });
    let status = tokio::process::Command::new(&binary)
        .args(&plan.args)
        .env_clear()
        .envs(child_env(&plan))
        .status()
        .await
        .with_context(|| format!("cannot start {}", binary.display()))?;
    if let Some(server) = owned {
        server.stop().await;
    }
    Ok(status.code().unwrap_or(1))
}

async fn print_env(tool: Option<&str>) -> Result<()> {
    let (config, _) = load_config()?;
    let url = config.local_url();
    let token = &config.server.auth_token;
    match tool.unwrap_or("claude") {
        "claude" => {
            println!("export ANTHROPIC_BASE_URL={url}");
            println!("export ANTHROPIC_AUTH_TOKEN={token}");
            println!("unset ANTHROPIC_API_KEY");
        }
        "codex" => {
            println!("# ~/.codex/config.toml");
            println!("model_provider = \"ferry\"");
            println!("model = {}", toml_string(&config.models.default));
            println!();
            println!("[model_providers.ferry]");
            println!("name = \"Ferry\"");
            println!("base_url = \"{url}/v1\"");
            println!("wire_api = \"responses\"");
            println!("env_key = \"FERRY_API_KEY\"");
            println!();
            println!("# and in your shell: export FERRY_API_KEY={token}");
        }
        other => bail!("unknown tool {other:?}; use claude or codex"),
    }
    if !existing_gateway(&url).await {
        eprintln!("note: nothing is listening on {url}; start the Ferry app or `ferry serve`");
    }
    Ok(())
}

fn print_models() -> Result<()> {
    let (config, _) = load_config()?;
    for model in ferry_proxy::router::advertised_models(&config) {
        println!("{model}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut config = Config::default();
        config.server.auth_token = "tok".into();
        config.models.default = "nim/meta/llama".into();
        config
    }

    #[test]
    fn codex_plan_injects_provider_before_user_args() {
        let plan = plan(
            Tool::Codex,
            "http://127.0.0.1:9",
            &config(),
            vec!["exec".into(), "hi".into()],
        );
        assert_eq!(plan.args[1], "model_provider=\"ferry\"");
        assert!(plan
            .args
            .contains(&"model_providers.ferry.base_url=\"http://127.0.0.1:9/v1\"".to_string()));
        assert!(plan.args.contains(&"model=\"nim/meta/llama\"".to_string()));
        assert_eq!(&plan.args[plan.args.len() - 2..], ["exec", "hi"]);
        assert_eq!(plan.set["FERRY_API_KEY"], "tok");
    }

    #[test]
    fn claude_plan_replaces_anthropic_env() {
        let plan = plan(Tool::Claude, "http://127.0.0.1:9", &config(), vec![]);
        assert_eq!(plan.set["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9");
        assert_eq!(plan.set["ANTHROPIC_AUTH_TOKEN"], "tok");
        assert_eq!(plan.remove_prefixes, ["ANTHROPIC_"]);
        let env = child_env(&plan);
        assert!(env
            .iter()
            .any(|(k, v)| k == "NO_PROXY" && v.contains("127.0.0.1")));
    }
}
