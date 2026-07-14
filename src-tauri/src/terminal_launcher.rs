use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

#[derive(Debug)]
pub struct TerminalLaunchSpec<'a> {
    pub executable: &'a Path,
    pub args: &'a [String],
    pub env: &'a BTreeMap<String, String>,
    pub work_dir: Option<&'a Path>,
}

#[cfg(unix)]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn unix_script(spec: &TerminalLaunchSpec<'_>) -> String {
    let mut script = String::from("#!/bin/sh\nset -eu\nrm -f -- \"$0\"\n");
    if let Some(work_dir) = spec.work_dir {
        script.push_str(&format!(
            "cd -- {}\n",
            shell_quote(&work_dir.to_string_lossy())
        ));
    }
    for (name, value) in spec.env {
        script.push_str(&format!("export {name}={}\n", shell_quote(value)));
    }
    script.push_str("exec ");
    script.push_str(&shell_quote(&spec.executable.to_string_lossy()));
    for arg in spec.args {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script.push('\n');
    script
}

#[cfg(unix)]
fn write_unix_script(spec: &TerminalLaunchSpec<'_>) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "macos")]
    let extension = "command";
    #[cfg(not(target_os = "macos"))]
    let extension = "sh";
    let path = std::env::temp_dir().join(format!(
        "trusted-carpool-launch-{}.{}",
        Uuid::new_v4(),
        extension
    ));
    std::fs::write(&path, unix_script(spec))
        .map_err(|error| format!("无法创建一次性终端启动文件: {error}"))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("无法保护一次性终端启动文件: {error}"))?;
    Ok(path)
}

fn cleanup_later(path: PathBuf) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(60));
        let _ = std::fs::remove_file(path);
    });
}

#[cfg(target_os = "macos")]
pub fn launch(spec: TerminalLaunchSpec<'_>) -> Result<(), String> {
    let script = write_unix_script(&spec)?;
    let result = Command::new("open")
        .args(["-a", "Terminal"])
        .arg(&script)
        .spawn()
        .map_err(|error| format!("无法打开 macOS 终端: {error}"));
    if result.is_err() {
        let _ = std::fs::remove_file(&script);
    } else {
        cleanup_later(script);
    }
    result.map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn launch(spec: TerminalLaunchSpec<'_>) -> Result<(), String> {
    let script = write_unix_script(&spec)?;
    let candidates: &[(&str, &[&str])] = &[
        ("x-terminal-emulator", &["-e"]),
        ("gnome-terminal", &["--"]),
        ("konsole", &["-e"]),
        ("xfce4-terminal", &["-x"]),
    ];
    for (terminal, args) in candidates {
        let mut command = Command::new(terminal);
        command.args(*args).arg(&script);
        match command.spawn() {
            Ok(_) => {
                cleanup_later(script);
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                let _ = std::fs::remove_file(&script);
                return Err(format!("无法打开 Linux 终端 {terminal}: {error}"));
            }
        }
    }
    let _ = std::fs::remove_file(&script);
    Err(
        "没有找到受支持的终端（x-terminal-emulator、GNOME Terminal、Konsole 或 Xfce Terminal）"
            .to_string(),
    )
}

#[cfg(any(target_os = "windows", test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(target_os = "windows", test))]
fn windows_script(spec: &TerminalLaunchSpec<'_>) -> String {
    let mut script = String::from("$ErrorActionPreference = 'Stop'\r\n");
    if let Some(work_dir) = spec.work_dir {
        script.push_str(&format!(
            "Set-Location -LiteralPath {}\r\n",
            powershell_quote(&work_dir.to_string_lossy())
        ));
    }
    for (name, value) in spec.env {
        script.push_str(&format!("$env:{} = {}\r\n", name, powershell_quote(value)));
    }
    script.push_str(
        "Remove-Item -LiteralPath $PSCommandPath -Force -ErrorAction SilentlyContinue\r\n& ",
    );
    script.push_str(&powershell_quote(&spec.executable.to_string_lossy()));
    for arg in spec.args {
        script.push(' ');
        script.push_str(&powershell_quote(arg));
    }
    script.push_str("\r\n");
    script
}

#[cfg(target_os = "windows")]
pub fn launch(spec: TerminalLaunchSpec<'_>) -> Result<(), String> {
    let path = std::env::temp_dir().join(format!("trusted-carpool-launch-{}.ps1", Uuid::new_v4()));
    std::fs::write(&path, windows_script(&spec))
        .map_err(|error| format!("无法创建一次性终端启动文件: {error}"))?;
    let result = Command::new("powershell.exe")
        .args(["-NoLogo", "-NoExit", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&path)
        .spawn()
        .map_err(|error| format!("无法打开 Windows PowerShell: {error}"));
    if result.is_err() {
        let _ = std::fs::remove_file(&path);
    } else {
        cleanup_later(path);
    }
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_launcher_quotes_paths_and_removes_secret_file_before_exec() {
        let env = BTreeMap::from([
            ("OPENAI_API_KEY".to_string(), "secret-value".to_string()),
            (
                "OPENAI_BASE_URL".to_string(),
                "http://127.0.0.1:25342/access/id/codex/v1".to_string(),
            ),
        ]);
        let args = vec!["-c".to_string(), "model='gpt test'".to_string()];
        let script = unix_script(&TerminalLaunchSpec {
            executable: Path::new("/tmp/a tool/codex"),
            args: &args,
            env: &env,
            work_dir: Some(Path::new("/tmp/project dir")),
        });
        assert!(script.contains("rm -f -- \"$0\""));
        assert!(script.contains("export OPENAI_API_KEY='secret-value'"));
        assert!(script.contains("exec '/tmp/a tool/codex'"));
        assert!(script.contains("cd -- '/tmp/project dir'"));
    }

    #[test]
    fn windows_launcher_quotes_paths_and_removes_secret_file_before_exec() {
        let env = BTreeMap::from([
            ("OPENAI_API_KEY".to_string(), "secret'value".to_string()),
            (
                "OPENAI_BASE_URL".to_string(),
                "http://127.0.0.1:25342/access/id/codex/v1".to_string(),
            ),
        ]);
        let args = vec!["-c".to_string(), "model='gpt test'".to_string()];
        let script = windows_script(&TerminalLaunchSpec {
            executable: Path::new(r"C:\Program Files\Codex\codex.exe"),
            args: &args,
            env: &env,
            work_dir: Some(Path::new(r"C:\Users\Friend\Project One")),
        });
        assert!(script.contains("Remove-Item -LiteralPath $PSCommandPath"));
        assert!(script.contains("$env:OPENAI_API_KEY = 'secret''value'"));
        assert!(script.contains("& 'C:\\Program Files\\Codex\\codex.exe'"));
        assert!(script.contains("Set-Location -LiteralPath 'C:\\Users\\Friend\\Project One'"));
    }
}
