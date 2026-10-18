//! Download and update Google's Antigravity ACP server from the ACP registry.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

/// Google's entry in the public ACP agent registry.
pub const REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json";
const REGISTRY_ENV: &str = "FERRY_ACP_REGISTRY_URL";
const INSTALLED_FILE: &str = "installed.json";

/// One downloadable build for the current platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    pub archive_url: String,
    pub cmd: String,
    pub args: Vec<String>,
    pub license_url: Option<String>,
}

/// What is unpacked on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
    /// Absolute path to the server executable.
    pub command: PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

pub fn registry_url() -> String {
    std::env::var(REGISTRY_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| REGISTRY_URL.to_string())
}

/// Registry key for this machine, e.g. `darwin-aarch64`.
pub fn platform_key() -> String {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    };
    format!("{os}-{}", std::env::consts::ARCH)
}

/// Pick this platform's build out of a registry entry.
pub fn parse_release(entry: &Value, platform: &str) -> Result<Release, String> {
    let version = entry["version"]
        .as_str()
        .ok_or("registry entry has no version")?
        .to_string();
    let build = entry
        .pointer("/distribution/binary")
        .and_then(|b| b.get(platform))
        .ok_or_else(|| format!("Antigravity ACP has no build for {platform}"))?;
    let archive_url = build["archive"]
        .as_str()
        .ok_or("registry build has no archive URL")?
        .to_string();
    let cmd = build["cmd"]
        .as_str()
        .ok_or("registry build has no command")?
        .to_string();
    let args = build["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        version,
        archive_url,
        cmd,
        args,
        license_url: entry["license_url"].as_str().map(str::to_string),
    })
}

pub async fn latest_release(http: &reqwest::Client) -> Result<Release, String> {
    let response = http
        .get(registry_url())
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("cannot reach the ACP registry: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("ACP registry returned HTTP {}", response.status()));
    }
    let entry: Value = response
        .json()
        .await
        .map_err(|e| format!("ACP registry entry is not JSON: {e}"))?;
    parse_release(&entry, &platform_key())
}

pub fn installed(root: &Path) -> Option<Installed> {
    let text = std::fs::read_to_string(root.join(INSTALLED_FILE)).ok()?;
    let installed: Installed = serde_json::from_str(&text).ok()?;
    installed.command.exists().then_some(installed)
}

/// Download `release` into `root/<version>`, then make it the active install
/// and delete older versions. The caller must stop any running server first.
pub async fn install(
    http: &reqwest::Client,
    root: &Path,
    release: &Release,
    on_progress: impl Fn(Progress) + Send,
) -> Result<Installed, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let archive = root.join(format!("download-{}.zip", release.version));
    download(http, &release.archive_url, &archive, on_progress).await?;

    let staging = root.join(format!(".staging-{}", release.version));
    let target = root.join(&release.version);
    let archive_for_unzip = archive.clone();
    let staging_for_unzip = staging.clone();
    tokio::task::spawn_blocking(move || unzip(&archive_for_unzip, &staging_for_unzip))
        .await
        .map_err(|e| format!("unpacking failed: {e}"))??;
    let _ = std::fs::remove_file(&archive);
    if target.exists() {
        std::fs::remove_dir_all(&target)
            .map_err(|e| format!("cannot replace {}: {e}", target.display()))?;
    }
    std::fs::rename(&staging, &target)
        .map_err(|e| format!("cannot move the new version in place: {e}"))?;

    let command = target.join(release.cmd.trim_start_matches("./"));
    if !command.exists() {
        return Err(format!("the archive does not contain {}", release.cmd));
    }
    make_executable(&command);
    let installed = Installed {
        version: release.version.clone(),
        command,
        args: release.args.clone(),
    };
    let marker = serde_json::to_string_pretty(&installed).map_err(|e| e.to_string())?;
    std::fs::write(root.join(INSTALLED_FILE), marker)
        .map_err(|e| format!("cannot record the install: {e}"))?;
    remove_other_versions(root, &release.version);
    Ok(installed)
}

async fn download(
    http: &reqwest::Client,
    url: &str,
    path: &Path,
    on_progress: impl Fn(Progress) + Send,
) -> Result<(), String> {
    let response = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "download failed: HTTP {} for {url}",
            response.status()
        ));
    }
    let total = response.content_length();
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    let mut downloaded = 0u64;
    let mut body = response.bytes_stream();
    let mut last_report = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("download interrupted: {e}"))?;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("cannot write the download: {e}"))?;
        downloaded += chunk.len() as u64;
        if downloaded - last_report >= 1 << 20 {
            last_report = downloaded;
            on_progress(Progress { downloaded, total });
        }
    }
    file.flush().await.map_err(|e| e.to_string())?;
    on_progress(Progress { downloaded, total });
    Ok(())
}

fn unzip(archive: &Path, into: &Path) -> Result<(), String> {
    if into.exists() {
        std::fs::remove_dir_all(into).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|e| format!("not a valid zip archive: {e}"))?;
    // `extract` rejects entries that would escape the target directory.
    zip.extract(into).map_err(|e| format!("cannot unpack: {e}"))
}

fn remove_other_versions(root: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && entry.file_name() != keep {
            if let Err(err) = std::fs::remove_dir_all(&path) {
                tracing::warn!(path = %path.display(), "cannot remove old version: {err}");
            }
        }
    }
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        let _ = std::fs::set_permissions(path, permissions);
    }
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_platform_build() {
        let entry = json!({
            "version": "1.4.1",
            "license_url": "https://antigravity.google/terms",
            "distribution": {"binary": {
                "linux-x86_64": {"archive": "https://dl/x.zip", "cmd": "./agy_acp_server.par", "args": ["--uid="]},
                "darwin-aarch64": {"archive": "https://dl/m.zip", "cmd": "./agy_acp_server.par"}
            }}
        });
        let release = parse_release(&entry, "linux-x86_64").unwrap();
        assert_eq!(release.version, "1.4.1");
        assert_eq!(release.args, ["--uid="]);
        assert!(parse_release(&entry, "darwin-aarch64")
            .unwrap()
            .args
            .is_empty());
        assert!(parse_release(&entry, "windows-x86_64").is_err());
    }
}
