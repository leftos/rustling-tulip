#Requires -Version 7.0
<#
.SYNOPSIS
    Launch the rustling-tulip Tauri app in dev mode.

.DESCRIPTION
    One-shot launcher for the desktop app. Builds the daemon binary if it
    isn't present (the Tauri build doesn't pull it in transitively), installs
    pnpm deps on first run, then starts `pnpm tauri dev`. Safe to re-run.

.PARAMETER Release
    Build the daemon (and Tauri) in release mode instead of debug.

.EXAMPLE
    .\launch.ps1
    .\launch.ps1 -Release
#>
[CmdletBinding()]
param(
    [switch]$Release
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir    = Join-Path $ScriptDir 'apps\tauri-app'
$Profile   = if ($Release) { 'release' } else { 'debug' }
$ExeName   = 'rustling-tulipd.exe'
$DaemonBin = Join-Path $ScriptDir "target\$Profile\$ExeName"

function Test-Tool($Name, $InstallHint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found on PATH. $InstallHint"
    }
}

Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
Test-Tool 'pnpm'  'Install pnpm via https://pnpm.io/installation or `npm install -g pnpm`.'

Write-Host "==> Building daemon ($Profile)..." -ForegroundColor Cyan
$cargoArgs = @('build', '-p', 'daemon')
if ($Release) { $cargoArgs += '--release' }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
if (-not (Test-Path $DaemonBin)) { throw "Daemon binary missing after build: $DaemonBin" }

Push-Location $AppDir
try {
    if (-not (Test-Path (Join-Path $AppDir 'node_modules'))) {
        Write-Host '==> Installing pnpm deps (first run)...' -ForegroundColor Cyan
        & pnpm install
        if ($LASTEXITCODE -ne 0) { throw "pnpm install failed (exit $LASTEXITCODE)" }
    }

    $tauriCmd = if ($Release) { @('tauri', 'dev', '--release') } else { @('tauri', 'dev') }
    Write-Host "==> Launching app ($Profile)..." -ForegroundColor Cyan
    & pnpm @tauriCmd @args
    exit $LASTEXITCODE
}
finally {
    Pop-Location
}
