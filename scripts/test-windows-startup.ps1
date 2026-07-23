[CmdletBinding()]
param(
  [string]$Executable = "src-tauri/target/release/trusted-carpool-desktop.exe",
  [int]$TimeoutSeconds = 30,
  [int]$SecondInstanceDelayMilliseconds = 100
)

$ErrorActionPreference = "Stop"
$exePath = (Resolve-Path $Executable).Path
$processName = [System.IO.Path]::GetFileNameWithoutExtension($exePath)
$appDataPath = Join-Path $env:APPDATA "ai.cnaigc.trusted-carpool"
$diagnosticPath = Join-Path $appDataPath "diagnostic-logs"
$expectedWindowTitle = "可信拼车"
$frontendReadyMessage = "frontend committed its first render"
$pageLoadedMessage = "main window finished loading"

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

function Get-DiagnosticMessageCount([string]$Message) {
  if (-not (Test-Path $diagnosticPath)) {
    return 0
  }

  $count = 0
  Get-ChildItem $diagnosticPath -Filter "diagnostics-*.jsonl" -File -ErrorAction SilentlyContinue |
    ForEach-Object {
      Get-Content $_.FullName -ErrorAction SilentlyContinue | ForEach-Object {
        try {
          $entry = $_ | ConvertFrom-Json -ErrorAction Stop
          if ($entry.source -eq "runtime" -and $entry.message -eq $Message) {
            $count += 1
          }
        }
        catch {
          # The writer may still be completing its final JSONL record. A later poll retries it.
        }
      }
    }
  return $count
}

function Write-RecentDiagnostics {
  if (-not (Test-Path $diagnosticPath)) {
    Write-Host "No diagnostic log directory was created at $diagnosticPath"
    return
  }

  Get-ChildItem $diagnosticPath -Filter "diagnostics-*.jsonl" -File -ErrorAction SilentlyContinue |
    Get-Content -ErrorAction SilentlyContinue |
    Select-Object -Last 100 |
    Write-Host
}

function Stop-TestProcess([System.Diagnostics.Process]$Process) {
  if ($null -eq $Process) {
    return
  }

  try {
    $Process.Refresh()
    if (-not $Process.HasExited) {
      $taskkill = Start-Process `
        -FilePath "taskkill.exe" `
        -ArgumentList @("/PID", [string]$Process.Id, "/T", "/F") `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
      if ($taskkill.ExitCode -ne 0) {
        $Process.Refresh()
        if (-not $Process.HasExited) {
          throw "taskkill failed to stop process tree $($Process.Id) with exit code $($taskkill.ExitCode)"
        }
      }
      if (-not $Process.WaitForExit(10000)) {
        throw "process tree $($Process.Id) did not exit within 10 seconds"
      }
    }
  }
  catch [System.InvalidOperationException] {
    # The process exited between Refresh and Stop-Process.
  }
}

function Start-TestProcess {
  return Start-Process `
    -FilePath $exePath `
    -WorkingDirectory $env:TEMP `
    -PassThru
}

function Assert-AccessibleWindow([System.Diagnostics.Process]$Process, [string]$Label) {
  $Process.Refresh()
  if ($Process.MainWindowHandle -eq 0) {
    throw "$Label launch has no main window handle"
  }
  if ($Process.MainWindowTitle -ne $expectedWindowTitle) {
    throw "$Label launch window title was '$($Process.MainWindowTitle)', expected '$expectedWindowTitle'"
  }

  $window = [System.Windows.Automation.AutomationElement]::FromHandle(
    [IntPtr]$Process.MainWindowHandle
  )
  if ($null -eq $window) {
    throw "$Label launch window is not discoverable through Windows UI Automation"
  }
  if ($window.Current.Name -ne $expectedWindowTitle) {
    throw "$Label launch UI Automation name was '$($window.Current.Name)', expected '$expectedWindowTitle'"
  }
  if ($window.Current.IsOffscreen) {
    throw "$Label launch window is still off-screen according to Windows UI Automation"
  }
  if (-not $window.Current.IsEnabled) {
    throw "$Label launch window is disabled according to Windows UI Automation"
  }

  # WebView2 accessibility descendants vary with the installed runtime. Report their
  # presence when available, but rely on the React-ready JSONL event plus the native
  # UIA window checks above rather than a runtime-specific descendant tree shape.
  try {
    $descendants = $window.FindAll(
      [System.Windows.Automation.TreeScope]::Descendants,
      [System.Windows.Automation.Condition]::TrueCondition
    )
    Write-Host "$Label launch UI Automation exposed $($descendants.Count) descendant element(s)."
  }
  catch {
    Write-Warning "$Label launch UI Automation descendants could not be enumerated: $($_.Exception.Message)"
  }
}

function Wait-ForReadyWindow(
  [System.Diagnostics.Process]$Process,
  [string]$Label,
  [int]$ReadyCountBefore,
  [int]$PageLoadedCountBefore,
  [bool]$AssertHiddenUntilReady = $false
) {
  $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)

  while ([DateTime]::UtcNow -lt $deadline) {
    Start-Sleep -Milliseconds 200
    $Process.Refresh()
    if ($Process.HasExited) {
      throw "$Label launch exited with code $($Process.ExitCode)"
    }

    $windowVisible = $Process.MainWindowHandle -ne 0
    $readyCount = Get-DiagnosticMessageCount $frontendReadyMessage
    $pageLoadedCount = Get-DiagnosticMessageCount $pageLoadedMessage

    # The backend records the React-ready event before it permits presentation. If a
    # rapid second launch reveals the hidden WebView first, this catches the regression.
    if ($AssertHiddenUntilReady -and $windowVisible -and $readyCount -le $ReadyCountBefore) {
      throw "$Label launch presented its window before the React-ready event was persisted"
    }

    if (
      $windowVisible -and
      $Process.Responding -and
      $readyCount -gt $ReadyCountBefore -and
      $pageLoadedCount -gt $PageLoadedCountBefore
    ) {
      Assert-AccessibleWindow $Process $Label
      return
    }
  }

  Write-RecentDiagnostics
  throw "$Label launch did not present a responsive, React-ready, UI Automation-visible window within $TimeoutSeconds seconds"
}

function Assert-DesktopStartup([string]$Label) {
  $readyCountBefore = Get-DiagnosticMessageCount $frontendReadyMessage
  $pageLoadedCountBefore = Get-DiagnosticMessageCount $pageLoadedMessage
  $process = Start-TestProcess

  try {
    Wait-ForReadyWindow `
      -Process $process `
      -Label $Label `
      -ReadyCountBefore $readyCountBefore `
      -PageLoadedCountBefore $pageLoadedCountBefore
  }
  finally {
    Stop-TestProcess $process
  }
}

function Assert-RapidSecondInstanceStartup {
  $readyCountBefore = Get-DiagnosticMessageCount $frontendReadyMessage
  $pageLoadedCountBefore = Get-DiagnosticMessageCount $pageLoadedMessage
  $primary = $null
  $secondary = $null

  try {
    $primary = Start-TestProcess
    Start-Sleep -Milliseconds $SecondInstanceDelayMilliseconds
    $secondary = Start-TestProcess

    Wait-ForReadyWindow `
      -Process $primary `
      -Label "rapid second-instance primary" `
      -ReadyCountBefore $readyCountBefore `
      -PageLoadedCountBefore $pageLoadedCountBefore `
      -AssertHiddenUntilReady $true

    $secondaryDeadline = [DateTime]::UtcNow.AddSeconds(10)
    while ([DateTime]::UtcNow -lt $secondaryDeadline) {
      $secondary.Refresh()
      if ($secondary.HasExited) {
        break
      }
      if ($secondary.MainWindowHandle -ne 0) {
        throw "rapid second instance created a visible window instead of handing off to the primary"
      }
      Start-Sleep -Milliseconds 100
    }
    $secondary.Refresh()
    if (-not $secondary.HasExited) {
      throw "rapid second instance did not exit after handing off to the primary"
    }

    $matchingProcesses = @(Get-Process $processName -ErrorAction SilentlyContinue)
    if ($matchingProcesses.Count -ne 1 -or $matchingProcesses[0].Id -ne $primary.Id) {
      $ids = ($matchingProcesses | ForEach-Object Id) -join ", "
      throw "expected only primary PID $($primary.Id) after rapid double launch; found: $ids"
    }
  }
  finally {
    Stop-TestProcess $secondary
    Stop-TestProcess $primary
  }
}

@(Get-Process $processName -ErrorAction SilentlyContinue) |
  ForEach-Object { Stop-TestProcess $_ }
if (@(Get-Process $processName -ErrorAction SilentlyContinue).Count -ne 0) {
  throw "existing $processName process trees remained after startup-test cleanup"
}
if (Test-Path $appDataPath) {
  Remove-Item $appDataPath -Recurse -Force
}

Assert-DesktopStartup "first"
Assert-DesktopStartup "second"
Assert-RapidSecondInstanceStartup
Write-Host "Windows first, second, and rapid single-instance startup smoke checks passed."
