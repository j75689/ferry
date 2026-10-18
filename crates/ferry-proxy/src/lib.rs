//! Ferry gateway: lets native Claude Code (Anthropic Messages API) and Codex
//! (OpenAI Responses API) run on any OpenAI-compatible or Anthropic-compatible
//! provider.

pub mod acp;
pub mod anthropic;
pub mod config;
pub mod local;
pub mod presets;
pub mod recovery;
pub mod responses;
pub mod router;
pub mod server;
pub mod stream;
pub mod upstream;

use std::net::SocketAddr;

pub use config::Config;
pub use server::AppState;

/// A gateway listening in the background of the current Tokio runtime.
pub struct RunningServer {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RunningServer {
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.task).await;
    }

    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

/// Bind `host:port` (port 0 picks a free one) and serve in the background.
pub async fn spawn(state: AppState, host: &str, port: u16) -> std::io::Result<RunningServer> {
    let listener = tokio::net::TcpListener::bind((host, port)).await?;
    let addr = listener.local_addr()?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server::serve(state, listener, async {
        let _ = rx.await;
    }));
    tracing::info!(%addr, "Ferry gateway listening");
    Ok(RunningServer {
        addr,
        shutdown: Some(tx),
        task,
    })
}
