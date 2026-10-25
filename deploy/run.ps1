# Keeps one claude-remote process running: restarts it if it exits, appends its output to a log.
# Started at logon by Task Scheduler (tasks "claude-remote-host" and "claude-remote-tunnel",
# registered by install-tasks.ps1).
#
#   run.ps1 host    -> claude-remote-host.exe (captures the Claude app; needs the logged-on desktop)
#   run.ps1 tunnel  -> cloudflared with %USERPROFILE%\.cloudflared\claude-remote.yml
param([Parameter(Mandatory)][ValidateSet('host', 'tunnel')][string]$What)

$dir = $PSScriptRoot
$log = Join-Path $dir "logs\$What.log"

switch ($What) {
    'host' {
        $exe = Join-Path $dir 'claude-remote-host.exe'
        $arguments = @()
    }
    'tunnel' {
        # The winget-managed copy, so `winget upgrade` keeps it current.
        $exe = "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\Cloudflare.cloudflared_Microsoft.Winget.Source_8wekyb3d8bbwe\cloudflared.exe"
        $arguments = @('tunnel', '--config', "$env:USERPROFILE\.cloudflared\claude-remote.yml", 'run')
    }
}

while ($true) {
    # Keep the log from growing without bound: one previous generation is kept.
    if ((Test-Path $log) -and (Get-Item $log).Length -gt 10MB) {
        Move-Item $log "$log.old" -Force
    }
    Add-Content $log "=== $(Get-Date -Format s) starting $What" -Encoding UTF8
    # Redirect through cmd so the programs' UTF-8 output lands in the log as is
    # (Windows PowerShell's own redirection would re-encode it as UTF-16).
    $quoted = ($arguments | ForEach-Object { "`"$_`"" }) -join ' '
    cmd.exe /c "`"`"$exe`" $quoted >> `"$log`" 2>&1`""
    Add-Content $log "=== $(Get-Date -Format s) $What exited with code $LASTEXITCODE; restarting in 5s" -Encoding UTF8
    # The host exits when the Claude app is not running yet; this retries until it is.
    Start-Sleep -Seconds 5
}
