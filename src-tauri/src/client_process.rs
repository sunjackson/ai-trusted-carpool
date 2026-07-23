use crate::models::ToolKind;
use std::fs;
#[cfg(any(target_os = "windows", test))]
use std::io::{Read, Result as IoResult};
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(target_os = "windows", test))]
use std::process::{Child, Output, Stdio};
#[cfg(any(target_os = "windows", test))]
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
#[cfg(any(target_os = "windows", test))]
use std::time::Instant;

#[cfg(target_os = "windows")]
use serde_json::Value;
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

#[cfg(target_os = "windows")]
const WINDOWS_PROCESS_QUERY_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(target_os = "windows")]
const WINDOWS_PROCESS_ACTION_TIMEOUT: Duration = Duration::from_secs(5);

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
        value
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()
    }
    #[cfg(not(target_os = "windows"))]
    {
        value.to_string_lossy().into_owned()
    }
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(target_os = "windows")]
pub(crate) fn terminate_process(pid: u32) -> Result<(), String> {
    let mut command = Command::new("taskkill.exe");
    command.args(["/PID", &pid.to_string(), "/T", "/F"]);
    hide_console_window(&mut command);
    match command_output_with_timeout(&mut command, WINDOWS_PROCESS_ACTION_TIMEOUT)
        .map_err(|error| format!("无法结束拼车客户端进程: {error}"))?
    {
        Some(output) if output.status.success() => Ok(()),
        Some(_) => Err("系统拒绝结束拼车客户端进程".to_string()),
        None => Err("结束拼车客户端进程超时".to_string()),
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn terminate_process(pid: u32) -> Result<(), String> {
    let pid_text = pid.to_string();
    let status = Command::new("kill")
        .args(["-TERM", &pid_text])
        .status()
        .map_err(|error| format!("无法结束拼车客户端进程: {error}"))?;
    if !status.success() {
        return Err("系统拒绝结束拼车客户端进程".to_string());
    }
    for _ in 0..5 {
        thread::sleep(Duration::from_millis(100));
        if list_processes()
            .map(|processes| processes.iter().all(|process| process.pid != pid))
            .unwrap_or(true)
        {
            return Ok(());
        }
    }
    let status = Command::new("kill")
        .args(["-KILL", &pid_text])
        .status()
        .map_err(|error| format!("无法强制结束拼车客户端进程: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "系统拒绝强制结束拼车客户端进程".to_string())
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
    let output = command_output_with_timeout(&mut command, WINDOWS_PROCESS_ACTION_TIMEOUT)
        .map_err(|error| format!("无法定位拼车客户端窗口: {error}"))?
        .ok_or_else(|| "定位拼车客户端窗口超时".to_string())?;
    output
        .status
        .success()
        .then_some(())
        .ok_or_else(|| "暂时无法定位拼车客户端窗口".to_string())
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
    let output = command_output_with_timeout(&mut command, WINDOWS_PROCESS_QUERY_TIMEOUT)
        .map_err(|error| format!("无法读取进程列表: {error}"))?
        .ok_or_else(|| "读取进程列表超时".to_string())?;
    if !output.status.success() {
        return Err("无法读取进程列表".to_string());
    }
    parse_windows_processes(&output.stdout)
}

#[cfg(any(target_os = "windows", test))]
fn read_pipe(mut pipe: impl Read) -> IoResult<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(any(target_os = "windows", test))]
fn remaining_timeout(started: Instant, timeout: Duration) -> Option<Duration> {
    timeout.checked_sub(started.elapsed())
}

#[cfg(target_os = "windows")]
struct WindowsProcessJob(OwnedHandle);

#[cfg(target_os = "windows")]
impl WindowsProcessJob {
    fn assign(child: &Child) -> IoResult<Self> {
        // SAFETY: null security/name pointers request a private job with default
        // security. The returned owned handle is closed exactly once.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: CreateJobObjectW returned a new owned HANDLE.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the information pointer and byte length describe `limits`,
        // which remains alive for the duration of the call.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(job))
    }

    fn terminate(&self) {
        // SAFETY: the job handle is owned by `self` and stays valid here.
        let _ = unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }
}

#[cfg(any(target_os = "windows", test))]
fn finish_child_in_background(mut child: Child) {
    thread::spawn(move || {
        #[cfg(target_os = "windows")]
        {
            let mut command = Command::new("taskkill.exe");
            command.args(["/PID", &child.id().to_string(), "/T", "/F"]);
            hide_console_window(&mut command);
            let _ = command.status();
        }
        #[cfg(all(test, unix))]
        {
            let mut command = Command::new("kill");
            command
                .args(["-KILL", &format!("-{}", child.id())])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = command.status();
        }
        let _ = child.kill();
        let _ = child.wait();
    });
}

#[cfg(any(target_os = "windows", test))]
fn command_output_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> IoResult<Option<Output>> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(all(test, unix))]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    #[cfg(target_os = "windows")]
    let process_job = match WindowsProcessJob::assign(&child) {
        Ok(job) => job,
        Err(error) => {
            finish_child_in_background(child);
            return Err(error);
        }
    };
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("child stdout is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("child stderr is unavailable"))?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = stdout_sender.send(read_pipe(stdout));
    });
    thread::spawn(move || {
        let _ = stderr_sender.send(read_pipe(stderr));
    });
    let started = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                finish_child_in_background(child);
                return Err(error);
            }
        }
        if started.elapsed() >= timeout {
            #[cfg(target_os = "windows")]
            process_job.terminate();
            finish_child_in_background(child);
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(25));
    };

    let Some(remaining) = remaining_timeout(started, timeout) else {
        #[cfg(target_os = "windows")]
        process_job.terminate();
        finish_child_in_background(child);
        return Ok(None);
    };
    let stdout = match stdout_receiver.recv_timeout(remaining) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            #[cfg(target_os = "windows")]
            process_job.terminate();
            finish_child_in_background(child);
            return Ok(None);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(std::io::Error::other("child stdout reader disconnected"));
        }
    };
    let Some(remaining) = remaining_timeout(started, timeout) else {
        #[cfg(target_os = "windows")]
        process_job.terminate();
        finish_child_in_background(child);
        return Ok(None);
    };
    let stderr = match stderr_receiver.recv_timeout(remaining) {
        Ok(result) => result?,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            #[cfg(target_os = "windows")]
            process_job.terminate();
            finish_child_in_background(child);
            return Ok(None);
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(std::io::Error::other("child stderr reader disconnected"));
        }
    };

    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
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

#[cfg(all(test, unix))]
mod command_tests {
    use super::*;

    #[test]
    fn bounded_command_output_stops_a_hung_child() {
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 2"]);

        let output = command_output_with_timeout(&mut command, Duration::from_millis(50))
            .expect("run bounded command");

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_command_output_kills_descendants_holding_inherited_pipes() {
        let temp = tempfile::TempDir::new().expect("temporary process marker");
        let pid_path = temp.path().join("descendant.pid");
        let started = Instant::now();
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("sleep 30 & echo $! > '{}'; exit 0", pid_path.display()),
        ]);

        let output = command_output_with_timeout(&mut command, Duration::from_millis(100))
            .expect("run bounded command");

        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(1));
        let pid = fs::read_to_string(&pid_path)
            .expect("read descendant pid")
            .trim()
            .to_string();
        for _ in 0..20 {
            let mut probe = Command::new("kill");
            probe
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if !probe.status().is_ok_and(|status| status.success()) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("descendant process {pid} survived the timeout");
    }

    #[test]
    fn bounded_command_output_collects_completed_output() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf stdout; printf stderr >&2"]);

        let output = command_output_with_timeout(&mut command, Duration::from_secs(1))
            .expect("run bounded command")
            .expect("command should complete");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"stdout");
        assert_eq!(output.stderr, b"stderr");
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_command_tests {
    use super::*;

    #[test]
    fn bounded_command_job_terminates_spawned_descendants() {
        let script = "$child=Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 30' -WindowStyle Hidden -PassThru; Write-Output $child.Id";
        let mut command = Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
        hide_console_window(&mut command);

        let output = command_output_with_timeout(&mut command, Duration::from_secs(5))
            .expect("run bounded PowerShell")
            .expect("parent PowerShell should finish");
        let pid = String::from_utf8(output.stdout)
            .expect("PowerShell pid output")
            .trim()
            .parse::<u32>()
            .expect("numeric descendant pid");

        for _ in 0..40 {
            let mut query = Command::new("powershell.exe");
            query.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }}"),
            ]);
            hide_console_window(&mut query);
            if query.status().is_ok_and(|status| status.success()) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }
        panic!("job-owned descendant process {pid} survived handle close");
    }
}
