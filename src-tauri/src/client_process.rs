use crate::models::ToolKind;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

#[cfg(target_os = "windows")]
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub(crate) pid: u32,
    pub(crate) executable: PathBuf,
    pub(crate) command_line: String,
    pub(crate) started_at: String,
}

pub(crate) fn profile_matches(process: &ProcessInfo, profile: Option<&Path>) -> bool {
    let Some(profile) = profile else {
        return true;
    };
    let profile = profile.to_string_lossy();
    let app_data = Path::new(profile.as_ref())
        .join("app-data")
        .to_string_lossy()
        .into_owned();
    process.command_line.contains(profile.as_ref()) || process.command_line.contains(&app_data)
}

pub(crate) fn process_belongs_to_tool(process: &ProcessInfo, kind: ToolKind) -> bool {
    let name = process
        .executable
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match kind {
        ToolKind::Claude => matches!(name.as_str(), "claude" | "claude.exe"),
        ToolKind::Codex => matches!(
            name.as_str(),
            "chatgpt" | "chatgpt.exe" | "codex" | "codex.exe" | "codex-desktop" | "codex-app"
        ),
    }
}

pub(crate) fn canonical_path_key(path: &Path) -> String {
    let value = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    #[cfg(target_os = "windows")]
    {
        return value
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
    }
    #[cfg(not(target_os = "windows"))]
    value.to_string_lossy().into_owned()
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(target_os = "windows")]
pub(crate) fn terminate_process(pid: u32) {
    let mut command = Command::new("taskkill.exe");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    hide_console_window(&mut command);
    let _ = command.status();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn terminate_process(pid: u32) {
    let pid_text = pid.to_string();
    let _ = Command::new("kill").args(["-TERM", &pid_text]).status();
    for _ in 0..5 {
        thread::sleep(Duration::from_millis(100));
        if list_processes()
            .map(|processes| processes.iter().all(|process| process.pid != pid))
            .unwrap_or(true)
        {
            return;
        }
    }
    let _ = Command::new("kill").args(["-KILL", &pid_text]).status();
}

#[cfg(target_os = "macos")]
pub(crate) fn focus_process(pid: u32) -> Result<(), String> {
    let script = format!(
        "tell application \"System Events\" to set frontmost of first process whose unix id is {pid} to true"
    );
    Command::new("osascript")
        .args(["-e", &script])
        .status()
        .map_err(|error| format!("无法定位拼车客户端窗口: {error}"))
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| "系统拒绝定位拼车客户端窗口，请检查辅助功能权限".to_string())
        })
}

#[cfg(target_os = "windows")]
pub(crate) fn focus_process(pid: u32) -> Result<(), String> {
    let script = format!(
        "$p=Get-Process -Id {pid} -ErrorAction Stop; Add-Type -TypeDefinition '[DllImport(\"user32.dll\")] public static extern bool SetForegroundWindow(IntPtr hWnd); [DllImport(\"user32.dll\")] public static extern bool ShowWindowAsync(IntPtr hWnd,int nCmdShow);' -Name Win32 -Namespace TCarpool; [TCarpool.Win32]::ShowWindowAsync($p.MainWindowHandle,9) | Out-Null; if(-not [TCarpool.Win32]::SetForegroundWindow($p.MainWindowHandle)){{exit 2}}"
    );
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", &script]);
    hide_console_window(&mut command);
    command
        .status()
        .map_err(|error| format!("无法定位拼车客户端窗口: {error}"))
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| "暂时无法定位拼车客户端窗口".to_string())
        })
}

#[cfg(target_os = "linux")]
pub(crate) fn focus_process(pid: u32) -> Result<(), String> {
    let status = Command::new("xdotool")
        .args([
            "search",
            "--onlyvisible",
            "--pid",
            &pid.to_string(),
            "windowactivate",
        ])
        .status();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(_) | Err(_) => Err("无法定位拼车客户端窗口，请确认桌面环境提供 xdotool".to_string()),
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let mut processes = Vec::new();
    let entries = fs::read_dir("/proc").map_err(|error| format!("无法读取进程列表: {error}"))?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let root = entry.path();
        let Ok(executable) = fs::read_link(root.join("exe")) else {
            continue;
        };
        let command_line = fs::read(root.join("cmdline"))
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .trim_end_matches('\0')
                    .replace('\0', " ")
            })
            .unwrap_or_default();
        let started_at = fs::read_to_string(root.join("stat"))
            .ok()
            .and_then(|stat| linux_process_start_marker(&stat))
            .unwrap_or_default();
        if !started_at.is_empty() {
            processes.push(ProcessInfo {
                pid,
                executable,
                command_line,
                started_at,
            });
        }
    }
    Ok(processes)
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn linux_process_start_marker(stat: &str) -> Option<String> {
    let after_name = stat.rsplit_once(')')?.1.trim();
    after_name
        .split_whitespace()
        .nth(19)
        .map(ToString::to_string)
}

#[cfg(target_os = "macos")]
pub(crate) fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,lstart=,command="])
        .output()
        .map_err(|error| format!("无法读取进程列表: {error}"))?;
    if !output.status.success() {
        return Err("无法读取进程列表".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_ps_process_line)
        .collect())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn parse_ps_process_line(line: &str) -> Option<ProcessInfo> {
    let fields = line.split_whitespace().collect::<Vec<_>>();
    if fields.len() < 7 {
        return None;
    }
    let pid = fields[0].parse().ok()?;
    let started_at = fields[1..6].join(" ");
    let command_line = fields[6..].join(" ");
    let executable = PathBuf::from(fields[6].trim_matches('"'));
    Some(ProcessInfo {
        pid,
        executable,
        command_line,
        started_at,
    })
}

#[cfg(target_os = "windows")]
pub(crate) fn list_processes() -> Result<Vec<ProcessInfo>, String> {
    let script = "Get-CimInstance Win32_Process | Select-Object ProcessId,ExecutablePath,CommandLine,CreationDate,Name | ConvertTo-Json -Compress";
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    hide_console_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("无法读取进程列表: {error}"))?;
    if !output.status.success() {
        return Err("无法读取进程列表".to_string());
    }
    parse_windows_processes(&output.stdout)
}

#[cfg(target_os = "windows")]
fn parse_windows_processes(bytes: &[u8]) -> Result<Vec<ProcessInfo>, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| format!("无法解析进程列表: {error}"))?;
    let values = match value {
        Value::Array(values) => values,
        value => vec![value],
    };
    Ok(values
        .into_iter()
        .filter_map(|value| {
            let pid = value.get("ProcessId")?.as_u64()?.try_into().ok()?;
            let executable = value
                .get("ExecutablePath")
                .and_then(Value::as_str)
                .or_else(|| value.get("Name").and_then(Value::as_str))?;
            let started_at = value.get("CreationDate")?.as_str()?.to_string();
            Some(ProcessInfo {
                pid,
                executable: PathBuf::from(executable),
                command_line: value
                    .get("CommandLine")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                started_at,
            })
        })
        .collect())
}
