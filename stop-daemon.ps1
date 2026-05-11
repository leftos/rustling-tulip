#Requires -Version 7.0
<#
.SYNOPSIS
    Stop every running rustling-tulip daemon process.

.DESCRIPTION
    Enumerates all processes named `rustling-tulipd` and terminates them.
    The image-name approach is intentional: when the daemon hangs and the
    user re-launches the app, a new daemon writes a fresh handshake with a
    new PID, leaving the original hung daemon nowhere recorded. Killing by
    name catches every instance, then cleans up the discovery file.

    No-op (exit 0) when nothing is running.

.EXAMPLE
    .\stop-daemon.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$HandshakeFile = Join-Path $env:APPDATA 'leftos\rustling-tulip\config\daemon.json'
$ImageName     = 'rustling-tulipd'

$processes = @(Get-Process -Name $ImageName -ErrorAction SilentlyContinue)
if ($processes.Count -eq 0) {
    Write-Host "No $ImageName processes running." -ForegroundColor Yellow
    if (Test-Path $HandshakeFile) {
        Remove-Item $HandshakeFile -Force
        Write-Host "Removed stale handshake." -ForegroundColor Yellow
    }
    exit 0
}

Write-Host "==> Stopping $($processes.Count) $ImageName process(es)..." -ForegroundColor Cyan
foreach ($p in $processes) {
    Write-Host "    killing PID $($p.Id)"
    Stop-Process -Id $p.Id -Force -ErrorAction Continue
}

# Give the OS up to 2s for handles to release before we report success.
for ($i = 0; $i -lt 20; $i++) {
    $remaining = @(Get-Process -Name $ImageName -ErrorAction SilentlyContinue)
    if ($remaining.Count -eq 0) { break }
    Start-Sleep -Milliseconds 100
}
$remaining = @(Get-Process -Name $ImageName -ErrorAction SilentlyContinue)
if ($remaining.Count -gt 0) {
    throw "Failed to stop $($remaining.Count) $ImageName process(es) within 2s: $($remaining.Id -join ', ')"
}

if (Test-Path $HandshakeFile) {
    Remove-Item $HandshakeFile -Force
}
Write-Host "==> Daemon(s) stopped." -ForegroundColor Green
exit 0
