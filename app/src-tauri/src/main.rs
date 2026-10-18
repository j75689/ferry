//! Ferry desktop app: hosts the gateway in-process, lives in the tray, and
//! edits the shared config file that the `ferry` CLI also reads.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use ferry_proxy::config::{ProviderConfig, ProviderKind};
use ferry_proxy::presets::{Preset, PRESETS};
use ferry_proxy::server::RequestRecord;
use ferry_proxy::{AppState, Config, RunningServer};
use serde::Serialize;
use serde_json::{json, Value};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State, WindowEvent};

struct Gateway {
    state: AppState,
    path: PathBuf,
    server: tokio::sync::Mutex<Option<RunningServer>>,
    /// Why the gateway is not listening, if it is not.
    error: Mutex<Option<String>>,
    /// Problem reading the config file at startup.
    load_error: Option<String>,
}

impl Gateway {
    async fn is_running(&self) -> bool {
        self.server
            .lock()
            .await
            .as_ref()
            .is_some_and(|s| !s.is_finished())
    }

    async fn restart(&self) {
        let mut server = self.server.lock().await;
        if let Some(running) = server.take() {
            running.stop().await;
        }
        let config = self.state.config();
        // A previous instance may still be releasing the port.
        let mut attempt = 0;
        let result = loop {
            match ferry_proxy::spawn(self.state.clone(), &config.server.host, config.server.port)
                .await
            {
                Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && attempt < 10 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                other => break other,
            }
        };
        match result {
            Ok(running) => {
                *server = Some(running);
                *self.error.lock().unwrap() = None;
            }
            Err(err) => {
                let message = format!(
                    "Cannot listen on {}:{} ({err}). Another program may be using this port; change it under Settings.",
                    config.server.host, config.server.port
                );
                tracing::error!("{message}");
                *self.error.lock().unwrap() = Some(message);
            }
        }
    }
}

#[derive(Serialize)]
struct Snapshot {
    config: Config,
    config_path: String,
    url: String,
    running: bool,
    error: Option<String>,
    presets: &'static [Preset],
    advertised_models: Vec<String>,
    version: &'static str,
}

async fn snapshot(gateway: &Gateway) -> Snapshot {
    let config = (*gateway.state.config()).clone();
    let running = gateway.is_running().await;
    let errors: Vec<String> = gateway
        .load_error
        .iter()
        .cloned()
        .chain(gateway.error.lock().unwrap().clone())
        .collect();
    Snapshot {
        url: config.local_url(),
        advertised_models: ferry_proxy::router::advertised_models(&config),
        config,
        config_path: gateway.path.display().to_string(),
        running,
        error: (!errors.is_empty()).then(|| errors.join("\n")),
        presets: PRESETS,
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
async fn get_snapshot(gateway: State<'_, Gateway>) -> Result<Snapshot, String> {
    Ok(snapshot(&gateway).await)
}

#[tauri::command]
async fn save_config(gateway: State<'_, Gateway>, config: Config) -> Result<Snapshot, String> {
    config.validate().map_err(|e| format!("{e:#}"))?;
    let previous = gateway.state.config();
    if gateway.load_error.is_some() && gateway.path.exists() {
        // The file we could not parse may still hold hand-written settings.
        let backup = gateway.path.with_extension("toml.bak");
        std::fs::copy(&gateway.path, &backup)
            .map_err(|e| format!("Cannot back up the old config file: {e}"))?;
    }
    config.save(&gateway.path).map_err(|e| format!("{e:#}"))?;
    let rebind =
        previous.server.host != config.server.host || previous.server.port != config.server.port;
    gateway.state.set_config(config);
    if rebind {
        gateway.restart().await;
    }
    Ok(snapshot(&gateway).await)
}

#[tauri::command]
async fn restart_gateway(gateway: State<'_, Gateway>) -> Result<Snapshot, String> {
    gateway.restart().await;
    Ok(snapshot(&gateway).await)
}

#[tauri::command]
fn recent_requests(gateway: State<'_, Gateway>) -> Vec<RequestRecord> {
    gateway.state.recent_requests()
}

#[tauri::command]
fn new_token() -> String {
    ferry_proxy::config::generate_token()
}

#[tauri::command]
async fn fetch_models(
    gateway: State<'_, Gateway>,
    provider: ProviderConfig,
) -> Result<Vec<String>, String> {
    ferry_proxy::upstream::list_models(gateway.state.http(), &provider)
        .await
        .map_err(|e| e.to_string())
}

/// Send a tiny real request so users can confirm key, URL and model at once.
#[tauri::command]
async fn test_provider(
    gateway: State<'_, Gateway>,
    provider: ProviderConfig,
    model: String,
) -> Result<String, String> {
    if model.trim().is_empty() {
        return Err("Enter a model name to test first".into());
    }
    let body = match provider.kind {
        ProviderKind::OpenaiChat => json!({
            "model": model,
            "max_tokens": 32,
            "stream": false,
            "messages": [{"role": "user", "content": "Reply with the single word: OK"}],
        }),
        ProviderKind::Anthropic => json!({
            "model": model,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "Reply with the single word: OK"}],
        }),
    };
    let started = std::time::Instant::now();
    let response = ferry_proxy::upstream::send(
        gateway.state.http(),
        &provider,
        &body,
        None,
        Duration::from_secs(60),
        0,
    )
    .await
    .map_err(|e| e.to_string())?;
    let value: Value = response
        .json()
        .await
        .map_err(|e| format!("Response is not JSON: {e}"))?;
    let text = value
        .pointer("/choices/0/message/content")
        .or_else(|| value.pointer("/content/0/text"))
        .and_then(Value::as_str)
        .unwrap_or("(empty response)")
        .trim()
        .chars()
        .take(120)
        .collect::<String>();
    Ok(format!(
        "OK in {} ms: {text}",
        started.elapsed().as_millis()
    ))
}

/// The `ferry` CLI ships next to the app executable as a Tauri sidecar.
fn bundled_cli() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("Cannot find the app directory")?;
    let path = dir.join(if cfg!(windows) { "ferry.exe" } else { "ferry" });
    if path.exists() {
        Ok(path)
    } else {
        Err(format!(
            "The bundled ferry CLI is missing ({}). In development, run cargo build -p ferry-cli first.",
            path.display()
        ))
    }
}

fn on_path(dir: &std::path::Path) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|p| p == dir))
}

/// Make `ferry` callable from any terminal; returns a message for the user.
#[tauri::command]
fn install_cli() -> Result<String, String> {
    let source = bundled_cli()?;
    let dir = cli_install_dir()?;
    if source.parent() == Some(dir.as_path()) || on_path(source.parent().unwrap_or(&dir)) {
        return Ok(format!(
            "ferry is already on PATH ({}); it is ready to use.",
            source.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let target = dir.join(source.file_name().unwrap_or_default());
    link_or_copy(&source, &target)?;
    let path_note = if on_path(&dir) {
        String::new()
    } else {
        add_to_path(&dir)?
    };
    Ok(format!(
        "Installed to {}. {path_note}Open a new terminal and run ferry claude or ferry codex.",
        target.display()
    ))
}

#[cfg(not(windows))]
fn cli_install_dir() -> Result<PathBuf, String> {
    Ok(dirs::home_dir()
        .ok_or("Cannot find the home directory")?
        .join(".local")
        .join("bin"))
}

#[cfg(windows)]
fn cli_install_dir() -> Result<PathBuf, String> {
    Ok(dirs::data_local_dir()
        .ok_or("Cannot find LocalAppData")?
        .join("ferry")
        .join("bin"))
}

#[cfg(unix)]
fn link_or_copy(source: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    // A symlink keeps following app updates.
    let _ = std::fs::remove_file(target);
    std::os::unix::fs::symlink(source, target)
        .map_err(|e| format!("Cannot create the symlink: {e}"))
}

#[cfg(windows)]
fn link_or_copy(source: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    std::fs::copy(source, target)
        .map(|_| ())
        .map_err(|e| format!("Cannot copy ferry.exe: {e}"))
}

#[cfg(not(windows))]
fn add_to_path(dir: &std::path::Path) -> Result<String, String> {
    Ok(format!(
        "Note: {} is not on PATH; add it in your shell profile (for example ~/.zshrc). ",
        dir.display()
    ))
}

#[cfg(windows)]
fn add_to_path(dir: &std::path::Path) -> Result<String, String> {
    // Append to the per-user PATH without the 1024-character limit of setx.
    let script = format!(
        "$d='{}'; $p=[Environment]::GetEnvironmentVariable('Path','User'); \
         if (-not ($p -split ';' | Where-Object {{ $_ -eq $d }})) {{ \
           [Environment]::SetEnvironmentVariable('Path', ($(if ($p) {{ $p + ';' }} else {{ '' }}) + $d), 'User') }}",
        dir.display().to_string().replace('\'', "''")
    );
    let status = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .map_err(|e| format!("Cannot update PATH: {e}"))?;
    if status.success() {
        Ok("Added to the user PATH. ".into())
    } else {
        Err("Updating PATH failed; add the folder to PATH manually.".into())
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FERRY_LOG")
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let path = Config::default_path();
    let (config, load_error) = match Config::load_or_init(&path) {
        Ok(config) => (config, None),
        Err(err) => (
            Config::default(),
            Some(format!(
                "Could not read the config file; using defaults for now: {err:#}"
            )),
        ),
    };
    let start_hidden = std::env::args().any(|a| a == "--hidden");
    let gateway = Gateway {
        state: AppState::new(config, load_error.is_none().then(|| path.clone())),
        path,
        server: tokio::sync::Mutex::new(None),
        error: Mutex::new(None),
        load_error,
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app)
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(gateway)
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            save_config,
            restart_gateway,
            recent_requests,
            new_token,
            fetch_models,
            test_provider,
            install_cli,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            tauri::async_runtime::block_on(handle.state::<Gateway>().restart());
            // Recover by itself once whatever held the port goes away.
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    let gateway = handle.state::<Gateway>();
                    if !gateway.is_running().await {
                        gateway.restart().await;
                    }
                }
            });

            let open = MenuItem::with_id(app, "open", "Open Ferry", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &quit])?;
            let mut tray = TrayIconBuilder::with_id("ferry")
                .tooltip("Ferry gateway")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;

            #[cfg(unix)]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    use tokio::signal::unix::{signal, SignalKind};
                    if let Ok(mut term) = signal(SignalKind::terminate()) {
                        term.recv().await;
                        handle.exit(0);
                    }
                });
            }

            if start_hidden {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the gateway running in the tray.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run Ferry desktop app");
}
