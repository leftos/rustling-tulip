#Requires -Version 7.0
<#
.SYNOPSIS
    Launch rustling-tulip — dev mode by default, standalone release build with -Release.

.DESCRIPTION
    Default (no -Release): runs `cargo build -p daemon` to bring the daemon
    binary up-to-date, then starts `pnpm tauri dev`. Cargo decides whether
    a rebuild is actually needed; the running daemon is only stopped when
    cargo recompiled (or the binary was locked). If the build fails because
    the daemon holds the binary lock, the running daemon is stopped and the
    build retried. `-ForceStopDaemon` skips that detection and stops the
    daemon up-front.

    With -Release: stops any running rustling-tulip processes, runs
    `pnpm build` to compile the frontend, runs `cargo build --release` to
    produce `target/release/rustling-tulipd.exe` and
    `target/release/rustling-tulip-app.exe`, then launches the app exe
    detached and exits. Lets you run the release build as a daily driver
    while iterating on debug builds without binary-lock conflicts. Release
    and debug share the same on-disk config dir under %APPDATA%, so your
    repos / workspaces / tabs carry over.

.PARAMETER Release
    Build the standalone release binaries and launch the release app exe
    detached. Without this flag the script runs `pnpm tauri dev` as before.

.PARAMETER ForceStopDaemon
    (Dev mode only.) Stop any running daemon before building, regardless
    of whether cargo needs to rebuild. Useful when you want to force a
    fresh daemon for reasons unrelated to source changes (e.g. clearing
    in-memory state). Ignored in -Release mode, which always stops first.

.EXAMPLE
    .\launch.ps1
    .\launch.ps1 -Release
    .\launch.ps1 -ForceStopDaemon
#>
[CmdletBinding()]
param(
    [switch]$Release,
    [switch]$ForceStopDaemon
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir    = Join-Path $ScriptDir 'apps\tauri-app'
$Profile   = if ($Release) { 'release' } else { 'debug' }
$ExeName   = 'rustling-tulipd.exe'
$DaemonBin = Join-Path $ScriptDir "target\$Profile\$ExeName"
$StopDaemonScript = Join-Path $ScriptDir 'stop-daemon.ps1'

function Test-Tool($Name, $InstallHint) {
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found on PATH. $InstallHint"
    }
}

# Probe whether the daemon binary can be opened for write. cargo fails to
# replace the exe on Windows when a process holds it; this is the most
# direct check (no string-matching cargo stderr, no race with `tasklist`).
function Test-DaemonBinaryWritable {
    param([string]$Path)
    if (-not (Test-Path $Path)) { return $true }
    try {
        $fs = [System.IO.File]::Open($Path, 'Open', 'Write', 'None')
        $fs.Close()
        return $true
    } catch {
        return $false
    }
}

function Stop-RunningDaemon {
    param([string]$Reason)
    $running = @(Get-Process -Name 'rustling-tulipd' -ErrorAction SilentlyContinue)
    if ($running.Count -eq 0) { return }
    Write-Host "==> Stopping $($running.Count) running daemon process(es) — $Reason..." -ForegroundColor Yellow
    # Clear $LASTEXITCODE so we don't see a stale value from a previous
    # external command (e.g. a failed cargo build). stop-daemon.ps1 throws
    # on failure and exits 0 on success.
    $global:LASTEXITCODE = 0
    & $StopDaemonScript
    if ($LASTEXITCODE -ne 0) { throw "stop-daemon.ps1 failed (exit $LASTEXITCODE)" }
}

# Run cargo build and capture output so we can detect whether it actually
# recompiled (vs a no-op pass). Streams to console via Tee-Object so the
# user still sees live progress.
function Invoke-DaemonBuild {
    param([string[]]$CargoArgs)
    $script:CargoLines = $null
    & cargo @CargoArgs 2>&1 | Tee-Object -Variable 'CargoLines' | Out-Default
    return $LASTEXITCODE
}

function Test-CargoRecompiled {
    param($Lines)
    if ($null -eq $Lines) { return $false }
    foreach ($line in $Lines) {
        if ($line -match '^\s*Compiling\s+(daemon|protocol)\b') { return $true }
    }
    return $false
}

Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
Test-Tool 'pnpm'  'Install pnpm via https://pnpm.io/installation or `npm install -g pnpm`.'

# ---------------------------------------------------------------------------
# Release-standalone path: build the embedded-frontend exes, launch detached,
# return. Lets the user run release as a daily driver while we keep iterating
# on debug builds without locking the running binaries.
# ---------------------------------------------------------------------------
if ($Release) {
    $appExe = Join-Path $ScriptDir 'target\release\rustling-tulip-app.exe'

    Write-Host '==> Stopping running rustling-tulip processes...' -ForegroundColor Cyan
    $killed = 0
    Get-Process -Name 'rustling-tulip-app', 'rustling-tulipd' -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "  killing $($_.ProcessName) (pid=$($_.Id))"
        Stop-Process -Id $_.Id -Force
        $killed++
    }
    if ($killed -eq 0) {
        Write-Host '  (none running)' -ForegroundColor DarkGray
    } else {
        # Let Windows release the exe locks before cargo tries to relink.
        Start-Sleep -Milliseconds 500
    }

    Push-Location $AppDir
    try {
        if (-not (Test-Path (Join-Path $AppDir 'node_modules'))) {
            Write-Host '==> Installing pnpm deps (first run)...' -ForegroundColor Cyan
            & pnpm install
            if ($LASTEXITCODE -ne 0) { throw "pnpm install failed (exit $LASTEXITCODE)" }
        }
        Write-Host '==> Building frontend bundle (pnpm build)...' -ForegroundColor Cyan
        & pnpm build
        if ($LASTEXITCODE -ne 0) { throw "pnpm build failed (exit $LASTEXITCODE)" }
    }
    finally {
        Pop-Location
    }

    Write-Host '==> Building daemon + tauri app (release)...' -ForegroundColor Cyan
    & cargo build --release --manifest-path (Join-Path $ScriptDir 'Cargo.toml') -p daemon -p rustling-tulip-app
    if ($LASTEXITCODE -ne 0) { throw "cargo build --release failed (exit $LASTEXITCODE)" }

    if (-not (Test-Path $appExe))    { throw "Release app exe missing: $appExe" }
    if (-not (Test-Path $DaemonBin)) { throw "Release daemon exe missing: $DaemonBin" }

    Write-Host "==> Launching $appExe" -ForegroundColor Cyan
    Start-Process -FilePath $appExe
    Write-Host 'rustling-tulip (release) started.' -ForegroundColor Green
    exit 0
}

if ($ForceStopDaemon) {
    Stop-RunningDaemon -Reason 'forced by -ForceStopDaemon'
}

Write-Host "==> Building daemon ($Profile)..." -ForegroundColor Cyan
$cargoArgs = @('build', '-p', 'daemon')

$buildExit = Invoke-DaemonBuild -CargoArgs $cargoArgs
$cargoOutput = $CargoLines

# If the build failed because the binary is locked by a running daemon,
# stop it and retry. This is the only case where we stop the daemon
# without first knowing a rebuild is needed — but a locked-binary failure
# from cargo proves a rebuild WAS needed (cargo wouldn't try to link
# otherwise).
if ($buildExit -ne 0 -and -not (Test-DaemonBinaryWritable $DaemonBin)) {
    Stop-RunningDaemon -Reason 'daemon binary locked, retrying build'
    Write-Host '==> Retrying daemon build...' -ForegroundColor Cyan
    $buildExit = Invoke-DaemonBuild -CargoArgs $cargoArgs
    $cargoOutput = $CargoLines
    $script:DaemonAlreadyStopped = $true
}

if ($buildExit -ne 0) { throw "cargo build failed (exit $buildExit)" }
if (-not (Test-Path $DaemonBin)) { throw "Daemon binary missing after build: $DaemonBin" }

# If cargo actually recompiled the daemon (or its deps), the running
# daemon is now stale — stop it so the Tauri app spawns a fresh one with
# the new binary. If it was a no-op, leave the running daemon alone.
if (-not $script:DaemonAlreadyStopped -and (Test-CargoRecompiled -Lines $cargoOutput)) {
    Stop-RunningDaemon -Reason 'daemon was rebuilt, restart needed'
} elseif (-not $ForceStopDaemon -and -not $script:DaemonAlreadyStopped) {
    Write-Host '==> Daemon up-to-date, leaving running instance alone.' -ForegroundColor DarkGray
}

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
