# Installs claude-remote to a fixed folder and registers two Task Scheduler tasks that start
# at logon under the current user: "claude-remote-host" and "claude-remote-tunnel".
# No administrator rights are needed.
#
# The host cannot be a Windows service: services run in session 0, which cannot capture the
# user's desktop. A logon task running as the user is the way to keep it up.
#
# Usage (after `cargo build --release`):
#   powershell -ExecutionPolicy Bypass -File deploy\install-tasks.ps1 -HostExe <path to claude-remote-host.exe>
#
# Keep InstallDir outside %LOCALAPPDATA% when running from a packaged (MSIX) app such as the
# Claude desktop app: its writes to AppData are redirected into the package's private folder,
# which Task Scheduler cannot see (the task then fails with 0x8007010B).
param(
    [Parameter(Mandatory)][string]$HostExe,
    [string]$InstallDir = "$env:USERPROFILE\claude-remote"
)

$ErrorActionPreference = 'Stop'
New-Item -ItemType Directory -Force (Join-Path $InstallDir 'logs') | Out-Null
Copy-Item $HostExe (Join-Path $InstallDir 'claude-remote-host.exe') -Force
Copy-Item (Join-Path $PSScriptRoot 'run.ps1') (Join-Path $InstallDir 'run.ps1') -Force

$user = "$env:USERDOMAIN\$env:USERNAME"
foreach ($what in 'host', 'tunnel') {
    # A headless console of its own: no window, and console signals (Ctrl+C / close) sent to
    # other consoles cannot reach it. A hidden powershell window was once taken down this way.
    $action = New-ScheduledTaskAction -Execute 'conhost.exe' `
        -Argument "--headless powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$InstallDir\run.ps1`" $what" `
        -WorkingDirectory $InstallDir
    # At logon, and every 5 minutes as a watchdog: with IgnoreNew a running instance is left
    # alone, and one that died is started again. (RestartCount only covers failures to start.)
    $triggers = @(
        (New-ScheduledTaskTrigger -AtLogOn -User $user),
        (New-ScheduledTaskTrigger -Once -At (Get-Date) -RepetitionInterval (New-TimeSpan -Minutes 5))
    )
    # Priority 4 = normal. The default (7) runs the task below normal priority, which made the
    # software encoder 2-3x slower whenever other apps were busy.
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries `
        -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 4 `
        -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
    $principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
    Register-ScheduledTask -TaskName "claude-remote-$what" -Description "claude-remote: keeps $what running (see $InstallDir\run.ps1)" `
        -Action $action -Trigger $triggers -Settings $settings -Principal $principal -Force | Out-Null
    Start-ScheduledTask -TaskName "claude-remote-$what"
    "registered and started claude-remote-$what"
}
