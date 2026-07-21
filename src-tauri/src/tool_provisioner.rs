//! Zero-dependency provisioning of the official Claude Code / Codex CLIs.
//!
//! Unlike the npm path (`tool_installer`), this module downloads the
//! official standalone binaries directly from the vendors' own release
//! channels, so a passenger machine needs neither Node.js nor admin rights:
//!
//! - Claude Code: `downloads.claude.ai` version manifest + raw binary,
//!   the exact flow used by the official `install.sh` / `install.ps1`.
//! - Codex: GitHub `openai/codex` release assets with per-asset SHA-256
//!   digests.
//!
//! Every download is streamed to a temp file, hash-verified against the
//! official checksum, unpacked when needed, and atomically activated under
//! `<app-data>/tools/<tool>/<version>/` with a `current` pointer file.
//! Binaries are never redistributed by us; each machine fetches them from
//! the official source.

use crate::models::ToolKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::Emitter;

pub const PROGRESS_EVENT: &str = "trusted-carpool:tool-install-progress";

const DEFAULT_CLAUDE_DOWNLOAD_BASE: &str = "https://downloads.claude.ai/claude-code-releases";
const DEFAULT_CODEX_RELEASE_API: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const RELEASE_CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const DOWNLOAD_DEADLINE: Duration = Duration::from_secs(20 * 60);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(60);
const PROGRESS_INTERVAL: Duration = Duration::from_millis(400);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn claude_download_base() -> String {
    std::env::var("TRUSTED_CARPOOL_CLAUDE_DOWNLOAD_BASE")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CLAUDE_DOWNLOAD_BASE.to_string())
}

fn codex_release_api() -> String {
    std::env::var("TRUSTED_CARPOOL_CODEX_RELEASE_API")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CODEX_RELEASE_API.to_string())
}

fn require_https(url: &str) -> Result<(), String> {
    if url.starts_with("https://")
        || cfg!(test)
        || std::env::var("TRUSTED_CARPOOL_ALLOW_HTTP").as_deref() == Ok("1")
    {
        Ok(())
    } else {
        Err(format!("下载地址必须使用 HTTPS: {url}"))
    }
}

// ---------------------------------------------------------------------------
// Platform mapping
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn linux_is_musl() -> bool {
    Path::new("/lib/libc.musl-x86_64.so.1").exists()
        || Path::new("/lib/libc.musl-aarch64.so.1").exists()
        || Path::new("/etc/alpine-release").exists()
}

pub fn claude_platform() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    {
        #[cfg(target_arch = "aarch64")]
        return Ok("darwin-arm64".to_string());
        #[cfg(target_arch = "x86_64")]
        return Ok("darwin-x64".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x64"
        };
        let suffix = if linux_is_musl() { "-musl" } else { "" };
        return Ok(format!("linux-{arch}{suffix}"));
    }
    #[cfg(target_os = "windows")]
    {
        #[cfg(target_arch = "aarch64")]
        return Ok("win32-arm64".to_string());
        #[cfg(not(target_arch = "aarch64"))]
        return Ok("win32-x64".to_string());
    }
    #[allow(unreachable_code)]
    Err("当前平台暂不支持一键下载 Claude Code".to_string())
}

pub fn codex_triple() -> Result<&'static str, String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    return Ok("aarch64-apple-darwin");
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    return Ok("x86_64-apple-darwin");
    // The musl builds are static, so they run on both glibc and musl distros.
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    return Ok("aarch64-unknown-linux-musl");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    return Ok("x86_64-unknown-linux-musl");
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    return Ok("aarch64-pc-windows-msvc");
    #[cfg(all(target_os = "windows", not(target_arch = "aarch64")))]
    return Ok("x86_64-pc-windows-msvc");
    #[allow(unreachable_code)]
    Err("当前平台暂不支持一键下载 Codex".to_string())
}

pub fn codex_asset_name(triple: &str) -> String {
    if triple.contains("windows") {
        format!("codex-{triple}.exe.zip")
    } else {
        format!("codex-{triple}.tar.gz")
    }
}

pub fn managed_binary_name(kind: ToolKind) -> &'static str {
    #[cfg(target_os = "windows")]
    {
        match kind {
            ToolKind::Claude => "claude.exe",
            ToolKind::Codex => "codex.exe",
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        kind.command()
    }
}

// ---------------------------------------------------------------------------
// Release metadata parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    RawBinary,
    TarGz,
    Zip,
}

#[derive(Debug, Clone)]
pub struct ProvisionPlan {
    pub version: String,
    pub download_url: String,
    pub sha256_hex: String,
    pub total_bytes: Option<u64>,
    pub archive: ArchiveKind,
}

#[derive(Debug, Deserialize)]
struct ClaudeManifest {
    version: String,
    platforms: HashMap<String, ClaudePlatformEntry>,
}

#[derive(Debug, Deserialize)]
struct ClaudePlatformEntry {
    binary: String,
    checksum: String,
    size: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: Option<u64>,
}

fn valid_version(value: &str) -> bool {
    let mut parts = value.split('.');
    matches!(
        (parts.next(), parts.next()),
        (Some(major), Some(_)) if major.chars().all(|c| c.is_ascii_digit()) && !major.is_empty()
    ) && value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

pub fn plan_from_claude_manifest(
    manifest_json: &str,
    platform: &str,
    base_url: &str,
) -> Result<ProvisionPlan, String> {
    let manifest: ClaudeManifest = serde_json::from_str(manifest_json)
        .map_err(|error| format!("Claude 官方版本清单格式无效: {error}"))?;
    if !valid_version(&manifest.version) {
        return Err("Claude 官方版本号无效".to_string());
    }
    let entry = manifest
        .platforms
        .get(platform)
        .ok_or_else(|| format!("Claude 官方版本清单缺少当前平台 {platform}"))?;
    let checksum = entry.checksum.trim().to_ascii_lowercase();
    if checksum.len() != 64 || !checksum.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Claude 官方校验值无效".to_string());
    }
    Ok(ProvisionPlan {
        download_url: format!(
            "{base_url}/{}/{platform}/{}",
            manifest.version, entry.binary
        ),
        version: manifest.version,
        sha256_hex: checksum,
        total_bytes: entry.size,
        archive: ArchiveKind::RawBinary,
    })
}

pub fn plan_from_codex_release(
    release_json: &str,
    asset_name: &str,
) -> Result<ProvisionPlan, String> {
    let release: GithubRelease = serde_json::from_str(release_json)
        .map_err(|error| format!("Codex 官方发布信息格式无效: {error}"))?;
    let version = release
        .tag_name
        .trim()
        .trim_start_matches("rust-v")
        .trim_start_matches('v')
        .to_string();
    if !valid_version(&version) {
        return Err(format!("Codex 官方版本号无效: {}", release.tag_name));
    }
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| format!("Codex 官方发布缺少当前平台资产 {asset_name}"))?;
    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .map(|digest| digest.trim().to_ascii_lowercase())
        .ok_or_else(|| "Codex 官方资产缺少 SHA-256 校验值，已拒绝下载".to_string())?;
    if digest.len() != 64 || !digest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("Codex 官方校验值无效".to_string());
    }
    let mut download_url = asset.browser_download_url.clone();
    if let Ok(mirror) = std::env::var("TRUSTED_CARPOOL_CODEX_DOWNLOAD_MIRROR") {
        let mirror = mirror.trim().trim_end_matches('/');
        if !mirror.is_empty() {
            if let Some(rest) = download_url.strip_prefix("https://github.com") {
                // The mirror only supplies bytes; the checksum still comes
                // from the official GitHub API, so a bad mirror cannot
                // substitute a different binary.
                download_url = format!("{mirror}{rest}");
            }
        }
    }
    Ok(ProvisionPlan {
        version,
        download_url,
        sha256_hex: digest,
        total_bytes: asset.size,
        archive: if asset_name.ends_with(".zip") {
            ArchiveKind::Zip
        } else {
            ArchiveKind::TarGz
        },
    })
}

/// Numeric, segment-wise version comparison ("2.1.10" > "2.1.9").
pub fn version_is_newer(candidate: &str, current: &str) -> bool {
    let numbers = |value: &str| -> Vec<u64> {
        value
            .split(['.', '-'])
            .map(|part| {
                part.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
            })
            .map(|digits| digits.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let candidate = numbers(candidate);
    let current = numbers(current);
    for index in 0..candidate.len().max(current.len()) {
        let left = candidate.get(index).copied().unwrap_or(0);
        let right = current.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Managed install layout
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ManagedTool {
    pub version: String,
    pub path: PathBuf,
}

pub fn managed_tool(root: &Path, kind: ToolKind) -> Option<ManagedTool> {
    let tool_dir = root.join(kind.command());
    let version = std::fs::read_to_string(tool_dir.join("current"))
        .ok()?
        .trim()
        .to_string();
    if !valid_version(&version) {
        return None;
    }
    let path = tool_dir.join(&version).join(managed_binary_name(kind));
    path.is_file().then_some(ManagedTool { version, path })
}

fn write_atomically(path: &Path, contents: &str) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, contents).map_err(|error| format!("无法写入 {temp:?}: {error}"))?;
    std::fs::rename(&temp, path).map_err(|error| format!("无法替换 {path:?}: {error}"))
}

/// Restricts a directory to the current user (no-op on Windows, where the
/// per-user app-data ACLs already apply).
fn secure_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("无法保护目录 {}: {error}", path.display()))?;
    }
    let _ = path;
    Ok(())
}

/// Streaming SHA-256 of a file on disk, hex-encoded.
pub fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("无法读取程序文件: {error}"))?;
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("无法读取程序文件: {error}"))?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(hex_encode(context.finish().as_ref()))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProvenanceRecord {
    kind: String,
    version: String,
    source_url: String,
    /// Checksum of the official download (archive for Codex, binary for Claude).
    sha256: String,
    /// Checksum of the activated binary itself, verified again before launch.
    binary_sha256: String,
    installed_at_ms: i64,
}

pub fn finalize_install(
    root: &Path,
    kind: ToolKind,
    plan: &ProvisionPlan,
    staged_binary: &Path,
) -> Result<PathBuf, String> {
    let binary_sha256 = sha256_file(staged_binary)?;
    let tool_dir = root.join(kind.command());
    let version_dir = tool_dir.join(&plan.version);
    std::fs::create_dir_all(&version_dir).map_err(|error| format!("无法创建安装目录: {error}"))?;
    secure_directory(root)?;
    secure_directory(&tool_dir)?;
    let target = version_dir.join(managed_binary_name(kind));
    let _ = std::fs::remove_file(&target);
    std::fs::rename(staged_binary, &target)
        .map_err(|error| format!("无法放置已下载的程序文件: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("无法设置可执行权限: {error}"))?;
    }
    write_atomically(&tool_dir.join("current"), &plan.version)?;
    let provenance = serde_json::to_string_pretty(&ProvenanceRecord {
        kind: kind.command().to_string(),
        version: plan.version.clone(),
        source_url: plan.download_url.clone(),
        sha256: plan.sha256_hex.clone(),
        binary_sha256,
        installed_at_ms: now_ms(),
    })
    .map_err(|error| format!("无法记录安装来源: {error}"))?;
    write_atomically(&tool_dir.join("provenance.json"), &provenance)?;
    prune_old_versions(&tool_dir, &plan.version);
    Ok(target)
}

/// Keeps the active version plus the most recent previous one (a manual
/// rollback path if a new release misbehaves); everything older is removed.
fn prune_old_versions(tool_dir: &Path, active_version: &str) {
    let Ok(entries) = std::fs::read_dir(tool_dir) else {
        return;
    };
    let mut previous: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| name != active_version && valid_version(name))
        .collect();
    previous.sort_by(|left, right| {
        if version_is_newer(left, right) {
            std::cmp::Ordering::Greater
        } else if version_is_newer(right, left) {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    });
    previous.pop(); // retain the newest previous version as a rollback path
    for stale in previous {
        let _ = std::fs::remove_dir_all(tool_dir.join(stale));
    }
}

/// Re-verifies an app-managed binary against its recorded checksum right
/// before launch. On any mismatch the `current` pointer is cleared so the UI
/// falls back to a fresh one-click install; the unexpected file stays on
/// disk for inspection but can no longer be launched through the app.
pub fn verify_managed_binary(root: &Path, kind: ToolKind) -> Result<(), String> {
    let quarantine = || {
        let _ = std::fs::remove_file(root.join(kind.command()).join("current"));
    };
    let Some(managed) = managed_tool(root, kind) else {
        return Err("托管的程序不存在，请重新一键安装".to_string());
    };
    let record: Option<ProvenanceRecord> =
        read_json(&root.join(kind.command()).join("provenance.json"));
    let expected = record
        .filter(|record| record.version == managed.version && record.kind == kind.command())
        .map(|record| record.binary_sha256);
    let Some(expected) = expected else {
        quarantine();
        return Err("缺少程序来源记录，已恢复为未安装状态，请重新一键安装".to_string());
    };
    if sha256_file(&managed.path)? != expected {
        quarantine();
        return Err(
            "程序完整性校验失败（文件与安装时不一致），已恢复为未安装状态，请重新一键安装"
                .to_string(),
        );
    }
    Ok(())
}

/// Runs `<binary> --version` with a hard timeout before a new download is
/// activated, so a broken release can never replace a working one.
pub fn smoke_test(binary: &Path) -> Result<(), String> {
    if std::env::var("TRUSTED_CARPOOL_SKIP_SMOKE_TEST").as_deref() == Ok("1") {
        return Ok(());
    }
    let mut command = std::process::Command::new(binary);
    command
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动下载的程序: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => return Err(format!("程序自检退出码异常: {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("程序自检超时".to_string());
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => return Err(format!("无法等待程序自检: {error}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Archive extraction
// ---------------------------------------------------------------------------

fn extract_tar_gz(archive: &Path, command: &str, target: &Path) -> Result<(), String> {
    let file =
        std::fs::File::open(archive).map_err(|error| format!("无法打开下载文件: {error}"))?;
    let mut tarball = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let entries = tarball
        .entries()
        .map_err(|error| format!("下载的压缩包无效: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("下载的压缩包无效: {error}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry
            .path()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        if name.starts_with(command) {
            entry
                .unpack(target)
                .map_err(|error| format!("无法解包程序文件: {error}"))?;
            return Ok(());
        }
    }
    Err("压缩包里没有找到可执行文件".to_string())
}

fn extract_zip(archive: &Path, command: &str, target: &Path) -> Result<(), String> {
    let file =
        std::fs::File::open(archive).map_err(|error| format!("无法打开下载文件: {error}"))?;
    let mut zipball =
        zip::ZipArchive::new(file).map_err(|error| format!("下载的压缩包无效: {error}"))?;
    for index in 0..zipball.len() {
        let mut entry = zipball
            .by_index(index)
            .map_err(|error| format!("下载的压缩包无效: {error}"))?;
        if !entry.is_file() {
            continue;
        }
        let name = entry
            .name()
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_string();
        if name.starts_with(command) {
            let mut output = std::fs::File::create(target)
                .map_err(|error| format!("无法解包程序文件: {error}"))?;
            std::io::copy(&mut entry, &mut output)
                .map_err(|error| format!("无法解包程序文件: {error}"))?;
            return Ok(());
        }
    }
    Err("压缩包里没有找到可执行文件".to_string())
}

pub fn extract_binary(
    archive: &Path,
    archive_kind: ArchiveKind,
    command: &str,
    target: &Path,
) -> Result<(), String> {
    match archive_kind {
        ArchiveKind::RawBinary => Err("原始二进制无需解包".to_string()),
        ArchiveKind::TarGz => extract_tar_gz(archive, command, target),
        ArchiveKind::Zip => extract_zip(archive, command, target),
    }
}

// ---------------------------------------------------------------------------
// Release caches (update checks without blocking detection)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionCache {
    version: String,
    fetched_at_ms: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseCache {
    payload_json: String,
    fetched_at_ms: i64,
}

fn cache_dir(root: &Path) -> PathBuf {
    root.join("cache")
}

fn claude_cache_path(root: &Path) -> PathBuf {
    cache_dir(root).join("claude-latest.json")
}

fn codex_cache_path(root: &Path) -> PathBuf {
    cache_dir(root).join("codex-release.json")
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("无法创建缓存目录: {error}"))?;
    }
    let contents =
        serde_json::to_string(value).map_err(|error| format!("无法编码缓存: {error}"))?;
    write_atomically(path, &contents)
}

/// Latest version known from local caches only; never touches the network.
pub fn latest_known_version(root: &Path, kind: ToolKind) -> Option<String> {
    match kind {
        ToolKind::Claude => {
            read_json::<VersionCache>(&claude_cache_path(root)).map(|cache| cache.version)
        }
        ToolKind::Codex => {
            let cache = read_json::<ReleaseCache>(&codex_cache_path(root))?;
            let release: GithubRelease = serde_json::from_str(&cache.payload_json).ok()?;
            let version = release
                .tag_name
                .trim()
                .trim_start_matches("rust-v")
                .trim_start_matches('v')
                .to_string();
            valid_version(&version).then_some(version)
        }
    }
}

/// Refreshes the release caches when stale. Runs in the background after
/// detection; failures are silent because caches are best-effort.
pub async fn refresh_release_caches(root: PathBuf) {
    let now = now_ms();
    let claude_stale = read_json::<VersionCache>(&claude_cache_path(&root))
        .map(|cache| now - cache.fetched_at_ms > RELEASE_CACHE_TTL_MS)
        .unwrap_or(true);
    if claude_stale {
        if let Ok(version) = fetch_claude_stable_version().await {
            let _ = write_json(
                &claude_cache_path(&root),
                &VersionCache {
                    version,
                    fetched_at_ms: now,
                },
            );
        }
    }
    let codex_stale = read_json::<ReleaseCache>(&codex_cache_path(&root))
        .map(|cache| now - cache.fetched_at_ms > RELEASE_CACHE_TTL_MS)
        .unwrap_or(true);
    if codex_stale {
        if let Ok(payload) = fetch_codex_release_json().await {
            let _ = write_json(
                &codex_cache_path(&root),
                &ReleaseCache {
                    payload_json: payload,
                    fetched_at_ms: now,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

fn http_client() -> Result<&'static reqwest::Client, String> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent("trusted-carpool-desktop")
                .connect_timeout(Duration::from_secs(15))
                .build()
                .ok()
        })
        .as_ref()
        .ok_or_else(|| "无法创建下载客户端".to_string())
}

async fn http_text(url: &str) -> Result<String, String> {
    require_https(url)?;
    let response = http_client()?
        .get(url)
        .header(
            "accept",
            "application/vnd.github+json, application/json, text/plain",
        )
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|error| format!("无法连接官方下载源: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("官方下载源返回 {}", response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("官方下载源响应无效: {error}"))
}

async fn fetch_claude_stable_version() -> Result<String, String> {
    let version = http_text(&format!("{}/stable", claude_download_base())).await?;
    let version = version.trim().to_string();
    if !valid_version(&version) {
        return Err("Claude 官方版本号无效，下载服务可能在当前地区不可用".to_string());
    }
    Ok(version)
}

async fn fetch_codex_release_json() -> Result<String, String> {
    http_text(&codex_release_api()).await
}

pub async fn resolve_plan(root: &Path, kind: ToolKind) -> Result<ProvisionPlan, String> {
    match kind {
        ToolKind::Claude => {
            let base = claude_download_base();
            let version = fetch_claude_stable_version().await?;
            let manifest = http_text(&format!("{base}/{version}/manifest.json")).await?;
            let _ = write_json(
                &claude_cache_path(root),
                &VersionCache {
                    version: version.clone(),
                    fetched_at_ms: now_ms(),
                },
            );
            let plan = plan_from_claude_manifest(&manifest, &claude_platform()?, &base)?;
            if plan.version != version {
                // The manifest is authoritative; the stable pointer may lag.
                return plan_from_claude_manifest(&manifest, &claude_platform()?, &base);
            }
            Ok(plan)
        }
        ToolKind::Codex => {
            // Installs always prefer freshly fetched official metadata; the
            // cache is only a fallback when GitHub is unreachable.
            let cached = read_json::<ReleaseCache>(&codex_cache_path(root));
            let payload = match fetch_codex_release_json().await {
                Ok(payload) => {
                    let _ = write_json(
                        &codex_cache_path(root),
                        &ReleaseCache {
                            payload_json: payload.clone(),
                            fetched_at_ms: now_ms(),
                        },
                    );
                    payload
                }
                Err(error) => match cached {
                    Some(cache) => cache.payload_json,
                    None => return Err(error),
                },
            };
            let plan = plan_from_codex_release(&payload, &codex_asset_name(codex_triple()?))?;
            require_https(&plan.download_url)?;
            Ok(plan)
        }
    }
}

// ---------------------------------------------------------------------------
// Cancellation + progress
// ---------------------------------------------------------------------------

fn cancel_registry() -> &'static Mutex<HashMap<&'static str, Arc<AtomicBool>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<&'static str, Arc<AtomicBool>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

struct CancelToken {
    key: &'static str,
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    fn register(kind: ToolKind) -> Result<Self, String> {
        let key = kind.command();
        let flag = Arc::new(AtomicBool::new(false));
        cancel_registry()
            .lock()
            .map_err(|_| "安装状态暂时不可用".to_string())?
            .insert(key, flag.clone());
        Ok(Self { key, flag })
    }

    fn cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        if let Ok(mut registry) = cancel_registry().lock() {
            registry.remove(self.key);
        }
    }
}

/// Requests cancellation of an in-flight download. Returns whether an active
/// provisioning task was found.
pub fn request_cancel(kind: ToolKind) -> bool {
    cancel_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.get(kind.command()).cloned())
        .map(|flag| {
            flag.store(true, Ordering::Relaxed);
            true
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    pub kind: ToolKind,
    pub phase: &'static str,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub version: Option<String>,
}

pub fn emit_progress<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    kind: ToolKind,
    phase: &'static str,
    received_bytes: u64,
    total_bytes: Option<u64>,
    version: Option<&str>,
) {
    let _ = app.emit(
        PROGRESS_EVENT,
        InstallProgress {
            kind,
            phase,
            received_bytes,
            total_bytes,
            version: version.map(str::to_string),
        },
    );
}

// ---------------------------------------------------------------------------
// Download + provision
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

async fn download_verified<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    kind: ToolKind,
    plan: &ProvisionPlan,
    destination: &Path,
    cancel: &CancelToken,
) -> Result<(), String> {
    require_https(&plan.download_url)?;
    let response = http_client()?
        .get(&plan.download_url)
        .send()
        .await
        .map_err(|error| format!("无法开始下载: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("官方下载源返回 {}", response.status()));
    }
    let total = response.content_length().or(plan.total_bytes);
    let mut response = response;
    let mut file =
        std::fs::File::create(destination).map_err(|error| format!("无法创建下载文件: {error}"))?;
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut received: u64 = 0;
    let deadline = Instant::now() + DOWNLOAD_DEADLINE;
    let mut last_progress = Instant::now();
    emit_progress(app, kind, "downloading", 0, total, Some(&plan.version));
    loop {
        if cancel.cancelled() {
            return Err("已取消安装".to_string());
        }
        if Instant::now() >= deadline {
            return Err("下载超时（超过 20 分钟），请检查网络后重试".to_string());
        }
        let chunk = tokio::time::timeout(CHUNK_TIMEOUT, response.chunk())
            .await
            .map_err(|_| "下载停滞（60 秒没有新数据），请检查网络后重试".to_string())?
            .map_err(|error| format!("下载中断: {error}"))?;
        let Some(chunk) = chunk else { break };
        digest.update(&chunk);
        file.write_all(&chunk)
            .map_err(|error| format!("无法写入下载文件: {error}"))?;
        received += chunk.len() as u64;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            emit_progress(
                app,
                kind,
                "downloading",
                received,
                total,
                Some(&plan.version),
            );
        }
    }
    file.flush()
        .map_err(|error| format!("无法写入下载文件: {error}"))?;
    drop(file);
    emit_progress(app, kind, "verifying", received, total, Some(&plan.version));
    let actual = hex_encode(digest.finish().as_ref());
    if actual != plan.sha256_hex {
        return Err("下载文件校验失败（内容与官方发布不一致），已删除。请重试".to_string());
    }
    Ok(())
}

fn cleanup(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Downloads, verifies, unpacks, and atomically activates the official
/// binary. Returns the installed version.
pub async fn provision<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    kind: ToolKind,
    root: &Path,
) -> Result<String, String> {
    let cancel = CancelToken::register(kind)?;
    emit_progress(app, kind, "resolving", 0, None, None);
    let plan = resolve_plan(root, kind).await?;
    let tmp_dir = root.join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(|error| format!("无法创建临时目录: {error}"))?;
    let download_path = tmp_dir.join(format!(
        "{}-{}.download",
        kind.command(),
        uuid::Uuid::new_v4()
    ));
    if let Err(error) = download_verified(app, kind, &plan, &download_path, &cancel).await {
        cleanup(&download_path);
        return Err(error);
    }
    emit_progress(
        app,
        kind,
        "installing",
        plan.total_bytes.unwrap_or(0),
        plan.total_bytes,
        Some(&plan.version),
    );
    let staged = if plan.archive == ArchiveKind::RawBinary {
        download_path.clone()
    } else {
        let staged = tmp_dir.join(format!("{}-{}.bin", kind.command(), uuid::Uuid::new_v4()));
        let archive_path = download_path.clone();
        let staged_path = staged.clone();
        let archive_kind = plan.archive;
        let command = kind.command();
        let extraction = tauri::async_runtime::spawn_blocking(move || {
            extract_binary(&archive_path, archive_kind, command, &staged_path)
        })
        .await
        .map_err(|error| format!("解包任务意外中断: {error}"))?;
        cleanup(&download_path);
        if let Err(error) = extraction {
            cleanup(&staged);
            return Err(error);
        }
        staged
    };
    if cancel.cancelled() {
        cleanup(&staged);
        return Err("已取消安装".to_string());
    }
    // The staged binary must prove it can run before it replaces anything.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let smoke_target = staged.clone();
    let smoke = tauri::async_runtime::spawn_blocking(move || smoke_test(&smoke_target))
        .await
        .map_err(|error| format!("程序自检意外中断: {error}"))?;
    if let Err(error) = smoke {
        cleanup(&staged);
        return Err(format!("下载的程序未通过自检，已放弃启用：{error}"));
    }
    match finalize_install(root, kind, &plan, &staged) {
        Ok(_) => Ok(plan.version),
        Err(error) => {
            cleanup(&staged);
            Err(error)
        }
    }
}

// ---------------------------------------------------------------------------
// App self-update awareness
// ---------------------------------------------------------------------------

const DEFAULT_APP_RELEASE_API: &str =
    "https://api.github.com/repos/sunjackson/ai-trusted-carpool/releases/latest";
pub const APP_RELEASES_PAGE: &str = "https://github.com/sunjackson/ai-trusted-carpool/releases";

fn app_release_api() -> String {
    std::env::var("TRUSTED_CARPOOL_APP_RELEASE_API")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_APP_RELEASE_API.to_string())
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppUpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
}

#[derive(Debug, Deserialize)]
struct AppRelease {
    tag_name: String,
}

pub fn app_update_from_release(
    release_json: &str,
    current_version: &str,
) -> Result<Option<AppUpdateInfo>, String> {
    let release: AppRelease = serde_json::from_str(release_json)
        .map_err(|error| format!("应用发布信息格式无效: {error}"))?;
    let latest = release.tag_name.trim().trim_start_matches('v').to_string();
    if !valid_version(&latest) {
        return Err(format!("应用发布版本号无效: {}", release.tag_name));
    }
    Ok(
        version_is_newer(&latest, current_version).then(|| AppUpdateInfo {
            current_version: current_version.to_string(),
            latest_version: format!("v{latest}"),
            // The opener always uses this pinned official page, never a
            // URL taken from the API response.
            release_url: APP_RELEASES_PAGE.to_string(),
        }),
    )
}

fn app_release_cache_path(root: &Path) -> PathBuf {
    cache_dir(root).join("app-release.json")
}

/// Compares the running app version against the latest GitHub release.
/// Results are cached for 24 hours; a stale cache is used when offline.
pub async fn check_app_update(
    root: &Path,
    current_version: &str,
) -> Result<Option<AppUpdateInfo>, String> {
    let cached = read_json::<ReleaseCache>(&app_release_cache_path(root));
    let fresh = cached
        .as_ref()
        .is_some_and(|cache| now_ms() - cache.fetched_at_ms <= RELEASE_CACHE_TTL_MS);
    let payload = if fresh {
        cached
            .as_ref()
            .map(|cache| cache.payload_json.clone())
            .unwrap_or_default()
    } else {
        match http_text(&app_release_api()).await {
            Ok(payload) => {
                let _ = write_json(
                    &app_release_cache_path(root),
                    &ReleaseCache {
                        payload_json: payload.clone(),
                        fetched_at_ms: now_ms(),
                    },
                );
                payload
            }
            Err(error) => match cached {
                Some(cache) => cache.payload_json,
                None => return Err(error),
            },
        }
    };
    app_update_from_release(&payload, current_version)
}

/// Opens the pinned official releases page in the system browser. The URL is
/// a compile-time constant, so nothing attacker-controlled can be launched.
pub fn open_releases_page() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(APP_RELEASES_PAGE);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(APP_RELEASES_PAGE);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", APP_RELEASES_PAGE]);
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开发布页面: {error}"))
}

pub fn manual_install_hint(kind: ToolKind) -> String {
    match kind {
        ToolKind::Claude => {
            "也可手动安装：npm install -g @anthropic-ai/claude-code，或参考 code.claude.com/docs"
                .to_string()
        }
        ToolKind::Codex => {
            "也可手动安装：npm install -g @openai/codex，或从 github.com/openai/codex/releases 下载"
                .to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_MANIFEST: &str = r#"{
        "version": "2.1.205",
        "platforms": {
            "darwin-arm64": {"binary": "claude", "checksum": "33e28624c5ae84f2bd7d2d8761e5d2e77997ba965cb11b6448de6b6e2c566f9c", "size": 237437968},
            "win32-x64": {"binary": "claude.exe", "checksum": "f09120889098672074e7c5166d5474da0c5482f2bec898b3510cacd9c1fefa42", "size": 247162528}
        }
    }"#;

    const CODEX_RELEASE: &str = r#"{
        "tag_name": "rust-v0.144.6",
        "assets": [
            {"name": "codex-aarch64-apple-darwin.tar.gz", "browser_download_url": "https://github.com/openai/codex/releases/download/rust-v0.144.6/codex-aarch64-apple-darwin.tar.gz", "digest": "sha256:023590f828bc9507ac61132ee35e74d3c5d33fb5ba3e1ca4fc2e013a2f71a3d7", "size": 98300000},
            {"name": "codex-x86_64-pc-windows-msvc.exe.zip", "browser_download_url": "https://github.com/openai/codex/releases/download/rust-v0.144.6/codex-x86_64-pc-windows-msvc.exe.zip", "digest": "sha256:0048604040fe61fa6163238fb0fcbda79e6bc465a8eecafc8f5ae8e4b69f77fd", "size": 114600000},
            {"name": "codex-no-digest.tar.gz", "browser_download_url": "https://github.com/openai/codex/releases/download/rust-v0.144.6/codex-no-digest.tar.gz", "digest": null, "size": 1}
        ]
    }"#;

    #[test]
    fn claude_manifest_resolves_platform_binary_and_checksum() {
        let plan = plan_from_claude_manifest(
            CLAUDE_MANIFEST,
            "darwin-arm64",
            "https://downloads.claude.ai/claude-code-releases",
        )
        .expect("plan");
        assert_eq!(plan.version, "2.1.205");
        assert_eq!(
            plan.download_url,
            "https://downloads.claude.ai/claude-code-releases/2.1.205/darwin-arm64/claude"
        );
        assert_eq!(plan.archive, ArchiveKind::RawBinary);
        assert_eq!(plan.total_bytes, Some(237_437_968));
        assert!(plan_from_claude_manifest(CLAUDE_MANIFEST, "linux-x64", "https://x").is_err());
    }

    #[test]
    fn codex_release_resolves_asset_digest_and_version() {
        let plan = plan_from_codex_release(CODEX_RELEASE, "codex-aarch64-apple-darwin.tar.gz")
            .expect("plan");
        assert_eq!(plan.version, "0.144.6");
        assert_eq!(plan.archive, ArchiveKind::TarGz);
        assert_eq!(
            plan.sha256_hex,
            "023590f828bc9507ac61132ee35e74d3c5d33fb5ba3e1ca4fc2e013a2f71a3d7"
        );
        let windows =
            plan_from_codex_release(CODEX_RELEASE, "codex-x86_64-pc-windows-msvc.exe.zip")
                .expect("windows plan");
        assert_eq!(windows.archive, ArchiveKind::Zip);
    }

    #[test]
    fn codex_assets_without_official_digests_are_rejected() {
        let error = plan_from_codex_release(CODEX_RELEASE, "codex-no-digest.tar.gz")
            .expect_err("must reject");
        assert!(error.contains("SHA-256"));
        assert!(plan_from_codex_release(CODEX_RELEASE, "missing-asset.tar.gz").is_err());
    }

    #[test]
    fn current_platform_maps_to_official_identifiers() {
        let platform = claude_platform().expect("claude platform");
        assert!([
            "darwin-arm64",
            "darwin-x64",
            "linux-x64",
            "linux-arm64",
            "linux-x64-musl",
            "linux-arm64-musl",
            "win32-x64",
            "win32-arm64",
        ]
        .contains(&platform.as_str()));
        let triple = codex_triple().expect("codex triple");
        assert!(codex_asset_name(triple).starts_with("codex-"));
        assert!(
            codex_asset_name("x86_64-pc-windows-msvc").ends_with(".exe.zip")
                && codex_asset_name("aarch64-apple-darwin").ends_with(".tar.gz")
        );
    }

    #[test]
    fn version_comparison_is_numeric_per_segment() {
        assert!(version_is_newer("2.1.10", "2.1.9"));
        assert!(version_is_newer("0.145.0", "0.144.6"));
        assert!(!version_is_newer("2.1.9", "2.1.10"));
        assert!(!version_is_newer("2.1.9", "2.1.9"));
        assert!(version_is_newer("2.2", "2.1.99"));
    }

    #[test]
    fn extracts_codex_binary_from_tar_gz() {
        let directory = tempfile::tempdir().expect("tempdir");
        let archive_path = directory.path().join("codex.tar.gz");
        let payload = b"#!codex-binary".to_vec();
        {
            let file = std::fs::File::create(&archive_path).expect("archive");
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(payload.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "codex-aarch64-apple-darwin",
                    payload.as_slice(),
                )
                .expect("append");
            builder
                .into_inner()
                .expect("finish")
                .finish()
                .expect("gzip");
        }
        let target = directory.path().join("codex");
        extract_binary(&archive_path, ArchiveKind::TarGz, "codex", &target).expect("extract");
        assert_eq!(std::fs::read(&target).expect("read"), payload);
    }

    #[test]
    fn extracts_codex_binary_from_zip() {
        let directory = tempfile::tempdir().expect("tempdir");
        let archive_path = directory.path().join("codex.zip");
        let payload = b"MZ-codex-windows".to_vec();
        {
            let file = std::fs::File::create(&archive_path).expect("archive");
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(
                    "codex-x86_64-pc-windows-msvc.exe",
                    zip::write::SimpleFileOptions::default(),
                )
                .expect("start");
            writer.write_all(&payload).expect("write");
            writer.finish().expect("finish");
        }
        let target = directory.path().join("codex.exe");
        extract_binary(&archive_path, ArchiveKind::Zip, "codex", &target).expect("extract");
        assert_eq!(std::fs::read(&target).expect("read"), payload);
    }

    #[test]
    fn finalize_activates_version_and_keeps_one_rollback_version() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let oldest_dir = root.join("codex/0.90.0");
        let previous_dir = root.join("codex/0.100.0");
        for (dir, content) in [(&oldest_dir, "oldest"), (&previous_dir, "previous")] {
            std::fs::create_dir_all(dir).expect("old dir");
            std::fs::write(dir.join(managed_binary_name(ToolKind::Codex)), content)
                .expect("old binary");
        }
        std::fs::write(root.join("codex/current"), "0.100.0").expect("old pointer");

        let staged = root.join("staged-codex");
        std::fs::write(&staged, "new-binary").expect("staged");
        let plan = ProvisionPlan {
            version: "0.144.6".to_string(),
            download_url: "https://github.com/openai/codex/releases/x".to_string(),
            sha256_hex: "ab".repeat(32),
            total_bytes: Some(10),
            archive: ArchiveKind::TarGz,
        };
        let installed = finalize_install(root, ToolKind::Codex, &plan, &staged).expect("finalize");
        assert!(installed.is_file());

        let managed = managed_tool(root, ToolKind::Codex).expect("managed");
        assert_eq!(managed.version, "0.144.6");
        assert_eq!(managed.path, installed);
        assert!(
            previous_dir.exists(),
            "the newest previous version is kept as a rollback path"
        );
        assert!(!oldest_dir.exists(), "older versions are pruned");
        let provenance =
            std::fs::read_to_string(root.join("codex/provenance.json")).expect("provenance");
        assert!(provenance.contains("0.144.6"));
        assert!(provenance.contains("binarySha256"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&installed)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }

    #[test]
    fn launch_integrity_check_quarantines_tampered_binaries() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let staged = root.join("staged-codex");
        std::fs::write(&staged, "official-bytes").expect("staged");
        let plan = ProvisionPlan {
            version: "0.144.6".to_string(),
            download_url: "https://github.com/openai/codex/releases/x".to_string(),
            sha256_hex: "ab".repeat(32),
            total_bytes: Some(10),
            archive: ArchiveKind::TarGz,
        };
        let installed = finalize_install(root, ToolKind::Codex, &plan, &staged).expect("finalize");
        verify_managed_binary(root, ToolKind::Codex).expect("pristine binary passes");

        std::fs::write(&installed, "tampered-bytes").expect("tamper");
        let error = verify_managed_binary(root, ToolKind::Codex).expect_err("must fail");
        assert!(error.contains("完整性校验失败"));
        assert!(
            managed_tool(root, ToolKind::Codex).is_none(),
            "the current pointer is cleared so the UI offers a fresh install"
        );
    }

    #[test]
    fn missing_provenance_also_blocks_managed_launches() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        let version_dir = root.join("claude/9.9.9");
        std::fs::create_dir_all(&version_dir).expect("dir");
        std::fs::write(version_dir.join(managed_binary_name(ToolKind::Claude)), "x")
            .expect("binary");
        std::fs::write(root.join("claude/current"), "9.9.9").expect("pointer");
        let error = verify_managed_binary(root, ToolKind::Claude).expect_err("must fail");
        assert!(error.contains("来源记录"));
        assert!(managed_tool(root, ToolKind::Claude).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn smoke_test_accepts_working_binaries_and_rejects_broken_ones() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let good = directory.path().join("good");
        std::fs::write(&good, "#!/bin/sh\nexit 0\n").expect("good script");
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        smoke_test(&good).expect("healthy binary passes");

        let bad = directory.path().join("bad");
        std::fs::write(&bad, "#!/bin/sh\nexit 3\n").expect("bad script");
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(smoke_test(&bad).is_err());
    }

    #[test]
    fn app_update_notice_appears_only_for_newer_official_releases() {
        let release = r#"{"tag_name": "v0.2.0"}"#;
        let update = app_update_from_release(release, "0.1.0")
            .expect("parse")
            .expect("newer release");
        assert_eq!(update.latest_version, "v0.2.0");
        assert_eq!(update.current_version, "0.1.0");
        assert_eq!(update.release_url, APP_RELEASES_PAGE);

        assert!(app_update_from_release(release, "0.2.0")
            .expect("parse")
            .is_none());
        assert!(app_update_from_release(release, "0.3.0")
            .expect("parse")
            .is_none());
        assert!(app_update_from_release(r#"{"tag_name": "nightly"}"#, "0.1.0").is_err());
    }

    #[test]
    fn managed_tool_requires_a_valid_pointer_and_binary() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        assert!(managed_tool(root, ToolKind::Claude).is_none());
        std::fs::create_dir_all(root.join("claude")).expect("dir");
        std::fs::write(root.join("claude/current"), "../escape").expect("pointer");
        assert!(managed_tool(root, ToolKind::Claude).is_none());
        std::fs::write(root.join("claude/current"), "9.9.9").expect("pointer");
        assert!(
            managed_tool(root, ToolKind::Claude).is_none(),
            "binary missing"
        );
    }

    #[test]
    fn cancel_requests_only_reach_active_installs() {
        assert!(!request_cancel(ToolKind::Claude));
        let token = CancelToken::register(ToolKind::Claude).expect("register");
        assert!(!token.cancelled());
        assert!(request_cancel(ToolKind::Claude));
        assert!(token.cancelled());
        drop(token);
        assert!(!request_cancel(ToolKind::Claude));
    }

    #[tokio::test]
    #[ignore = "downloads ~100MB from the official GitHub release; run manually"]
    async fn provisions_codex_end_to_end_and_binary_runs() {
        let app = tauri::test::mock_app();
        let directory = tempfile::tempdir().expect("tempdir");
        let version = provision(app.handle(), ToolKind::Codex, directory.path())
            .await
            .expect("provision codex");
        let managed = managed_tool(directory.path(), ToolKind::Codex).expect("managed");
        assert_eq!(managed.version, version);
        let output = std::process::Command::new(&managed.path)
            .arg("--version")
            .output()
            .expect("run downloaded codex");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains(&version), "unexpected --version: {stdout}");
    }

    #[tokio::test]
    #[ignore = "requires network; verifies the official release endpoints end to end"]
    async fn official_endpoints_resolve_to_verified_plans() {
        let directory = tempfile::tempdir().expect("tempdir");
        let claude = resolve_plan(directory.path(), ToolKind::Claude)
            .await
            .expect("claude plan");
        assert_eq!(claude.archive, ArchiveKind::RawBinary);
        assert_eq!(claude.sha256_hex.len(), 64);
        assert!(claude
            .download_url
            .starts_with("https://downloads.claude.ai/claude-code-releases/"));
        assert!(claude.total_bytes.unwrap_or(0) > 50_000_000);

        let codex = resolve_plan(directory.path(), ToolKind::Codex)
            .await
            .expect("codex plan");
        assert_eq!(codex.sha256_hex.len(), 64);
        assert!(codex
            .download_url
            .starts_with("https://github.com/openai/codex/releases/download/"));
        assert!(codex.total_bytes.unwrap_or(0) > 20_000_000);
        assert_eq!(
            latest_known_version(directory.path(), ToolKind::Codex),
            Some(codex.version.clone())
        );
    }

    #[test]
    fn cached_release_metadata_answers_update_checks_offline() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        assert!(latest_known_version(root, ToolKind::Codex).is_none());
        write_json(
            &codex_cache_path(root),
            &ReleaseCache {
                payload_json: CODEX_RELEASE.to_string(),
                fetched_at_ms: now_ms(),
            },
        )
        .expect("cache");
        assert_eq!(
            latest_known_version(root, ToolKind::Codex),
            Some("0.144.6".to_string())
        );
        write_json(
            &claude_cache_path(root),
            &VersionCache {
                version: "2.1.205".to_string(),
                fetched_at_ms: now_ms(),
            },
        )
        .expect("cache");
        assert_eq!(
            latest_known_version(root, ToolKind::Claude),
            Some("2.1.205".to_string())
        );
    }
}
