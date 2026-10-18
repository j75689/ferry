//! Google Antigravity through its official ACP server (`agy_acp_server`).
//!
//! Ferry installs the server from the ACP registry, starts it on demand,
//! lets the user sign in through it, and drives it as a model provider.

pub mod bridge;
pub mod client;
pub mod install;
pub mod mcp;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use client::Connection;
use install::{Installed, Progress, Release};

const DATA_ENV: &str = "FERRY_DATA_DIR";
/// Cold starts of the official server take up to ~25 s on Windows.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(90);
/// Browser-based sign-in waits for the user.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub installed_version: Option<String>,
    pub running: bool,
    pub platform: String,
    /// From the agent's `initialize` result once it has been started.
    pub auth_methods: Vec<Value>,
    pub agent_info: Value,
    pub error: Option<String>,
}

pub struct AcpManager {
    root: PathBuf,
    http: reqwest::Client,
    connection: tokio::sync::Mutex<Option<Arc<Connection>>>,
    last_error: std::sync::Mutex<Option<String>>,
    pub(crate) mcp: mcp::McpServer,
}

impl AcpManager {
    pub fn new(root: PathBuf, http: reqwest::Client) -> Self {
        Self {
            root,
            http,
            connection: tokio::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            mcp: mcp::McpServer::default(),
        }
    }

    /// `$FERRY_DATA_DIR/antigravity-acp`, or the platform's local data dir.
    pub fn default_root() -> PathBuf {
        let base = std::env::var(DATA_ENV)
            .ok()
            .filter(|p| !p.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::data_local_dir().map(|d| d.join("ferry")))
            .unwrap_or_else(|| PathBuf::from(".ferry"));
        base.join("antigravity-acp")
    }

    /// An empty working directory, so the agent's own tools find nothing.
    pub fn sandbox_dir(&self) -> PathBuf {
        let dir = self.root.join("workspace");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    pub fn installed(&self) -> Option<Installed> {
        install::installed(&self.root)
    }

    pub async fn connection(&self) -> Result<Arc<Connection>, String> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref().filter(|c| c.is_alive()) {
            return Ok(connection.clone());
        }
        let installed = self
            .installed()
            .ok_or("the Antigravity ACP server is not installed; install it in the Ferry app")?;
        match Connection::spawn(&installed, STARTUP_TIMEOUT).await {
            Ok(connection) => {
                *self.last_error.lock().unwrap() = None;
                *slot = Some(connection.clone());
                Ok(connection)
            }
            Err(err) => {
                *self.last_error.lock().unwrap() = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Shut down `broken` if it is still the current agent process, so the
    /// next request starts a fresh one.
    pub async fn discard(&self, broken: &Arc<Connection>) {
        let mut slot = self.connection.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, broken)) {
            tracing::warn!("restarting the Antigravity agent");
            if let Some(connection) = slot.take() {
                connection.shutdown().await;
            }
        }
    }

    pub async fn stop(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.shutdown().await;
        }
    }

    /// Current state; starts the server (if installed) to learn its auth methods.
    pub async fn status(&self, start: bool) -> Status {
        let installed = self.installed();
        let connection = match (&installed, start) {
            (Some(_), true) => self.connection().await.ok(),
            _ => self
                .connection
                .lock()
                .await
                .clone()
                .filter(|c| c.is_alive()),
        };
        Status {
            installed_version: installed.map(|i| i.version),
            running: connection.is_some(),
            platform: install::platform_key(),
            auth_methods: connection
                .as_ref()
                .map(|c| c.auth_methods())
                .unwrap_or_default(),
            agent_info: connection
                .as_ref()
                .map(|c| c.initialize()["agentInfo"].clone())
                .unwrap_or(Value::Null),
            error: self.last_error.lock().unwrap().clone(),
        }
    }

    pub async fn latest(&self) -> Result<Release, String> {
        install::latest_release(&self.http).await
    }

    /// Install or update to the registry's latest version.
    pub async fn install_latest(
        &self,
        on_progress: impl Fn(Progress) + Send,
    ) -> Result<Installed, String> {
        let release = self.latest().await?;
        self.stop().await;
        install::install(&self.http, &self.root, &release, on_progress).await
    }

    pub async fn authenticate(&self, method_id: &str) -> Result<(), String> {
        let connection = self.connection().await?;
        connection
            .request("authenticate", json!({"methodId": method_id}), AUTH_TIMEOUT)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Models the signed-in account can use. Fails when sign-in is needed.
    pub async fn models(&self) -> Result<Vec<bridge::ModelOption>, String> {
        let connection = self.connection().await?;
        let session = connection
            .request(
                "session/new",
                json!({"cwd": self.sandbox_dir().display().to_string(), "mcpServers": []}),
                Duration::from_secs(60),
            )
            .await
            .map_err(|e| bridge::rpc_failure(&e).message)?;
        if let Some(session_id) = session["sessionId"].as_str() {
            if connection
                .capability("/sessionCapabilities/close")
                .is_some()
            {
                let _ = connection
                    .request(
                        "session/close",
                        json!({"sessionId": session_id}),
                        Duration::from_secs(10),
                    )
                    .await;
            }
        }
        Ok(bridge::model_options(&session).1)
    }
}
