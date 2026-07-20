//! One-click installation of the official Claude Code / Codex CLIs.
//!
//! CC Switch style: the app never bundles the CLIs. Installation delegates to
//! the official npm packages, using the same GUI-safe executable discovery as
//! detection so it works when the desktop app is launched outside a shell.

use crate::models::ToolKind;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
const STDERR_TAIL_CHARS: usize = 800;

static ACTIVE_INSTALLS: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

pub fn npm_package(kind: ToolKind) -> &'static str {
    match kind {
        ToolKind::Claude => "@anthropic-ai/claude-code",
        ToolKind::Codex => "@openai/codex",
    }
}

pub fn manual_install_command(kind: ToolKind) -> String {
    format!("npm install -g {}", npm_package(kind))
}

fn install_args(kind: ToolKind) -> Vec<String> {
    vec![
        "install".to_string(),
        "-g".to_string(),
        "--no-fund".to_string(),
        "--no-audit".to_string(),
        "--loglevel=error".to_string(),
        npm_package(kind).to_string(),
    ]
}

fn npm_version_from(package_json: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(package_json).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    Some(format!("v{}", value.get("version")?.as_str()?))
}

/// Best-effort version lookup for npm-managed installs by resolving the bin
/// shim to its package directory. It reads `package.json` instead of running
/// `<tool> --version`, so detection stays instant. Native (non-npm) installs
/// simply report no version.
pub fn installed_version(kind: ToolKind, executable: &Path) -> Option<String> {
    let package = Path::new(npm_package(kind));
    let mut candidates = Vec::new();
    if let Some(parent) = executable.parent() {
        // Windows npm prefix: <prefix>\claude.cmd + <prefix>\node_modules\<pkg>
        candidates.push(parent.join("node_modules").join(package));
        // Unix npm prefix: <prefix>/bin/claude -> <prefix>/lib/node_modules/<pkg>
        candidates.push(parent.join("../lib/node_modules").join(package));
    }
    // Symlinked bins (npm, pnpm, volta shims) resolve inside the package itself.
    if let Ok(resolved) = std::fs::canonicalize(executable) {
        for ancestor in resolved.ancestors() {
            if ancestor.ends_with(package) {
                candidates.push(ancestor.to_path_buf());
            }
        }
    }
    candidates
        .iter()
        .find_map(|directory| npm_version_from(&directory.join("package.json")))
}

/// Serializes installs per tool so double-clicks and parallel windows cannot
/// spawn two npm processes for the same package.
pub struct InstallGuard(&'static str);

impl InstallGuard {
    pub fn acquire(kind: ToolKind) -> Result<Self, String> {
        let command = kind.command();
        let mut active = ACTIVE_INSTALLS
            .lock()
            .map_err(|_| "安装状态暂时不可用".to_string())?;
        if active.contains(&command) {
            return Err("这个工具正在安装中，请稍候".to_string());
        }
        active.push(command);
        Ok(Self(command))
    }
}

impl Drop for InstallGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = ACTIVE_INSTALLS.lock() {
            active.retain(|command| *command != self.0);
        }
    }
}

fn stderr_tail(output: &str) -> String {
    let trimmed = output.trim();
    if trimmed.chars().count() <= STDERR_TAIL_CHARS {
        return trimmed.to_string();
    }
    let skip = trimmed.chars().count() - STDERR_TAIL_CHARS;
    trimmed.chars().skip(skip).collect()
}

fn failure_message(kind: ToolKind, stderr: &str) -> String {
    let lowered = stderr.to_ascii_lowercase();
    let hint = if lowered.contains("eacces") || lowered.contains("permission denied") {
        "npm 全局目录没有写入权限。建议改用 nvm/fnm 安装 Node.js，或参考 npm 官方文档调整全局目录后重试。"
    } else if lowered.contains("enotfound")
        || lowered.contains("etimedout")
        || lowered.contains("econnreset")
        || lowered.contains("network")
    {
        "网络连接失败。请检查网络（必要时配置 npm 镜像或代理）后重试。"
    } else {
        "可以在终端手动执行下面的命令查看完整原因。"
    };
    format!(
        "安装 {} 失败：{}\n手动安装命令：{}\n{}",
        npm_package(kind),
        stderr_tail(stderr),
        manual_install_command(kind),
        hint
    )
}

/// Runs `npm install -g <package>` with npm's own directory prepended to
/// PATH, so npm's `#!/usr/bin/env node` shebang resolves the matching Node
/// even when the desktop app inherited a minimal GUI PATH.
pub fn install(kind: ToolKind, npm: &Path) -> Result<(), String> {
    let mut command = Command::new(npm);
    command
        .args(install_args(kind))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(npm_dir) = npm.parent() {
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![npm_dir.to_path_buf()];
        paths.extend(std::env::split_paths(&existing));
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("PATH", joined);
        }
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动 npm 安装进程: {error}"))?;
    let stderr_pipe = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut buffer = String::new();
        if let Some(mut stderr) = stderr_pipe {
            let _ = stderr.read_to_string(&mut buffer);
        }
        buffer
    });

    let deadline = Instant::now() + INSTALL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "安装超时（超过 {} 分钟）。请检查网络后重试，或手动执行：{}",
                        INSTALL_TIMEOUT.as_secs() / 60,
                        manual_install_command(kind)
                    ));
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            Err(error) => return Err(format!("无法等待安装进程: {error}")),
        }
    };
    let stderr_output = stderr_reader.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(failure_message(kind, &stderr_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_npm_packages_are_used_for_each_tool() {
        assert_eq!(npm_package(ToolKind::Claude), "@anthropic-ai/claude-code");
        assert_eq!(npm_package(ToolKind::Codex), "@openai/codex");
        assert_eq!(
            manual_install_command(ToolKind::Claude),
            "npm install -g @anthropic-ai/claude-code"
        );
    }

    #[test]
    fn install_args_target_a_quiet_global_install() {
        let args = install_args(ToolKind::Codex);
        assert_eq!(args[0], "install");
        assert!(args.contains(&"-g".to_string()));
        assert!(args.contains(&"--loglevel=error".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("@openai/codex"));
    }

    #[test]
    fn concurrent_installs_of_the_same_tool_are_rejected() {
        let guard = InstallGuard::acquire(ToolKind::Claude).expect("first acquire");
        assert!(InstallGuard::acquire(ToolKind::Claude).is_err());
        // A different tool may install in parallel.
        let codex_guard = InstallGuard::acquire(ToolKind::Codex).expect("other tool");
        drop(codex_guard);
        drop(guard);
        let reacquired = InstallGuard::acquire(ToolKind::Claude).expect("after release");
        drop(reacquired);
    }

    #[test]
    fn failure_messages_translate_common_npm_errors() {
        let permission = failure_message(ToolKind::Claude, "npm error EACCES: permission denied");
        assert!(permission.contains("没有写入权限"));
        assert!(permission.contains("npm install -g @anthropic-ai/claude-code"));

        let network = failure_message(ToolKind::Codex, "npm error ETIMEDOUT registry.npmjs.org");
        assert!(network.contains("网络连接失败"));

        let generic = failure_message(ToolKind::Codex, "npm error something unexpected");
        assert!(generic.contains("手动安装命令"));
    }

    #[test]
    fn long_stderr_output_is_trimmed_to_a_readable_tail() {
        let noisy = "x".repeat(5_000);
        assert_eq!(stderr_tail(&noisy).chars().count(), STDERR_TAIL_CHARS);
        assert_eq!(stderr_tail("  short  "), "short");
    }

    #[test]
    fn version_is_read_from_a_windows_style_npm_prefix() {
        let prefix = tempfile::tempdir().expect("tempdir");
        let package_dir = prefix.path().join("node_modules/@openai/codex");
        std::fs::create_dir_all(&package_dir).expect("package dir");
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"@openai/codex","version":"0.140.0"}"#,
        )
        .expect("package.json");
        let shim = prefix.path().join("codex.cmd");
        std::fs::write(&shim, "@echo off").expect("shim");
        assert_eq!(
            installed_version(ToolKind::Codex, &shim),
            Some("v0.140.0".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn version_is_read_through_a_unix_npm_bin_symlink() {
        let prefix = tempfile::tempdir().expect("tempdir");
        let package_dir = prefix
            .path()
            .join("lib/node_modules/@anthropic-ai/claude-code");
        std::fs::create_dir_all(&package_dir).expect("package dir");
        std::fs::write(
            package_dir.join("package.json"),
            r#"{"name":"@anthropic-ai/claude-code","version":"2.1.178"}"#,
        )
        .expect("package.json");
        std::fs::write(package_dir.join("cli.js"), "#!/usr/bin/env node").expect("cli");
        let bin_dir = prefix.path().join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin dir");
        let shim = bin_dir.join("claude");
        std::os::unix::fs::symlink(package_dir.join("cli.js"), &shim).expect("symlink");
        assert_eq!(
            installed_version(ToolKind::Claude, &shim),
            Some("v2.1.178".to_string())
        );
    }

    #[test]
    fn native_installs_without_npm_metadata_report_no_version() {
        let directory = tempfile::tempdir().expect("tempdir");
        let binary = directory.path().join("claude");
        std::fs::write(&binary, "binary").expect("binary");
        assert_eq!(installed_version(ToolKind::Claude, &binary), None);
    }
}
