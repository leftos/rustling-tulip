#Requires -Version 7.0
<#
.SYNOPSIS
    Rustling-tulip dev helper -- build, launch, package, test, lint, format,
    clean, and manage the daemon process. One entry point replaces the
    older `launch.ps1` and `stop-daemon.ps1`.

.DESCRIPTION
    Subcommands:

      run        Build the daemon (cargo) then launch `pnpm tauri dev`. This
                 is the default when no subcommand is given -- same flow as
                 the old `launch.ps1`. With -Release: build the full release
                 artifacts and launch the standalone exe detached.
      build      Build only -- no app launch. Debug by default; with -Release
                 produces the standalone-app artifacts (release daemon,
                 frontend bundle, and `rustling-tulip-app.exe`).
      launch     Launch only -- assumes binaries are already built. In debug
                 this runs `pnpm tauri dev`; with -Release it starts the
                 already-built `rustling-tulip-app.exe` detached.
      installer  Run `pnpm tauri build` to produce installer bundles
                 (`target\release\bundle\msi\*.msi` and `nsis\*.exe`).
      stop       Kill any running daemon process(es) and clean the stale
                 handshake file. No-op when nothing is running.
      restart    `stop` followed by `run`. Use this for "fresh dev loop"
                 after the daemon got into a bad state.
      test       Run `cargo test` across the workspace.
      clippy     Run the strict workspace clippy pass
                 (`--all-targets --all-features -- -D warnings`).
      fmt        Run `cargo fmt --all`.
      clean      Run `cargo clean`.
      help       Print the subcommand summary.

.PARAMETER Command
    The subcommand to run (positional). When omitted, defaults to `run`.

.PARAMETER Release
    Applies to `build`, `launch`, `run`, and `restart`. Selects the release
    profile instead of debug.

.PARAMETER ForceStopDaemon
    Applies to `run`/`restart` only. Stops any running daemon before
    building, regardless of whether cargo decides a rebuild is needed.

.EXAMPLE
    .\rt.ps1                  # = .\rt.ps1 run
    .\rt.ps1 build -Release
    .\rt.ps1 launch
    .\rt.ps1 installer
    .\rt.ps1 stop
    .\rt.ps1 restart
    .\rt.ps1 clippy
#>
[CmdletBinding()]
# Write-Host is intentional: this is an interactive dev script and the colored
# status lines are how the user sees progress. Same UX as the predecessor
# launch.ps1 / stop-daemon.ps1 it replaces.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Interactive dev script; colored status to console is the UX.')]
# $Release, $ForceStopDaemon, $Rest are read by sub-functions via the script
# scope; PSScriptAnalyzer's parameter-usage check doesn't trace that.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '',
    Justification = 'Top-level params consumed by sub-functions via $script: scope.')]
param(
    [Parameter(Position = 0)]
    [ValidateSet('', 'run', 'build', 'launch', 'installer', 'stop', 'restart', 'test', 'clippy', 'fmt', 'clean', 'help')]
    [string]$Command = '',

    [switch]$Release,
    [switch]$ForceStopDaemon,

    # Extra arguments forwarded to the underlying tool (e.g. `cargo test --
    # mytest`). Only meaningful for `run`, `test`, `clippy`, `fmt`.
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

$ScriptDir     = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir        = Join-Path $ScriptDir 'apps\tauri-app'
$ManifestPath  = Join-Path $ScriptDir 'Cargo.toml'
$ImageName     = 'rustling-tulipd'
$AppImageName  = 'rustling-tulip-app'
$HandshakeFile = Join-Path $env:APPDATA 'leftos\rustling-tulip\config\daemon.json'

function Get-DaemonBin {
    # `$Profile` is a PowerShell automatic variable for the user's profile
    # path; use `$BuildProfile` here to avoid the collision.
    param([string]$BuildProfile)
    Join-Path $ScriptDir "target\$BuildProfile\$ImageName.exe"
}

function Get-AppExe {
    Join-Path $ScriptDir "target\release\$AppImageName.exe"
}

# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

function Test-Tool {
    param([string]$Name, [string]$InstallHint)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found on PATH. $InstallHint"
    }
}

function Assert-Tooling {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Asserts presence of multiple tools; plural noun is accurate.')]
    param()
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Test-Tool 'pnpm'  'Install pnpm via https://pnpm.io/installation or `npm install -g pnpm`.'
}

function Test-CargoExitOk {
    param([string]$What)
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
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

function Stop-DaemonProcesses {
    [CmdletBinding()]
    [OutputType([bool])]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Stops zero-or-more processes; plural noun is accurate.')]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'Dev script: the subcommand itself is the user gesture; no extra prompt needed.')]
    param([string]$Reason)
    $processes = @(Get-Process -Name $ImageName -ErrorAction SilentlyContinue)
    if ($processes.Count -eq 0) {
        return $false
    }
    $msg = if ($Reason) { "==> Stopping $($processes.Count) $ImageName process(es) -- $Reason..." }
           else        { "==> Stopping $($processes.Count) $ImageName process(es)..." }
    Write-Host $msg -ForegroundColor Yellow
    foreach ($p in $processes) {
        Write-Host "    killing PID $($p.Id)"
        Stop-Process -Id $p.Id -Force -ErrorAction Continue
    }
    # Give the OS up to 2s for handles to release.
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
    return $true
}

# Run cargo build and capture output so we can detect whether it actually
# recompiled (vs a no-op pass). Streams to console via Tee-Object so the
# user still sees live progress.
function Invoke-CargoCapture {
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

function Initialize-FrontendDeps {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Initializes the full set of frontend deps; plural noun is accurate.')]
    param()
    if (-not (Test-Path (Join-Path $AppDir 'node_modules'))) {
        Write-Host '==> Installing pnpm deps (first run)...' -ForegroundColor Cyan
        Push-Location $AppDir
        try {
            & pnpm install
            Test-CargoExitOk 'pnpm install'
        } finally {
            Pop-Location
        }
    }
}

# ---------------------------------------------------------------------------
# Builders
# ---------------------------------------------------------------------------

# Debug daemon-only build with the smart "locked binary" retry from the old
# launch.ps1. Sets $script:DaemonAlreadyStopped when the running daemon was
# killed to break the lock, so the caller knows not to redundantly stop it.
function Invoke-DebugBuild {
    Write-Host '==> Building daemon (debug)...' -ForegroundColor Cyan
    $cargoArgs = @('build', '-p', 'daemon')

    $exit = Invoke-CargoCapture -CargoArgs $cargoArgs
    $output = $script:CargoLines
    $script:DaemonAlreadyStopped = $false

    if ($exit -ne 0 -and -not (Test-DaemonBinaryWritable (Get-DaemonBin -BuildProfile 'debug'))) {
        [void](Stop-DaemonProcesses -Reason 'daemon binary locked, retrying build')
        Write-Host '==> Retrying daemon build...' -ForegroundColor Cyan
        $exit = Invoke-CargoCapture -CargoArgs $cargoArgs
        $output = $script:CargoLines
        $script:DaemonAlreadyStopped = $true
    }

    Test-CargoExitOk 'cargo build'
    if (-not (Test-Path (Get-DaemonBin -BuildProfile 'debug'))) {
        throw "Daemon binary missing after build: $(Get-DaemonBin -BuildProfile 'debug')"
    }
    $script:CargoRecompiledDaemon = Test-CargoRecompiled -Lines $output
}

# Release build of daemon + standalone app exe. `--features
# rustling-tulip-app/custom-protocol` is required: Tauri's build.rs sets
# `cfg(dev)` to the negation of that feature, and `cfg(dev)` makes the
# runtime load the frontend from `devUrl` (localhost:1420) instead of the
# embedded `frontendDist` bundle. `tauri build` enables this automatically;
# raw `cargo build` does not -- without it the launched release exe shows
# "localhost refused to connect".
function Invoke-ReleaseBuild {
    Write-Host '==> Stopping running rustling-tulip processes (release rebuild needs the exe lock)...' -ForegroundColor Cyan
    $killed = 0
    Get-Process -Name $AppImageName, $ImageName -ErrorAction SilentlyContinue | ForEach-Object {
        Write-Host "    killing $($_.ProcessName) (pid=$($_.Id))"
        Stop-Process -Id $_.Id -Force
        $killed++
    }
    if ($killed -eq 0) {
        Write-Host '    (none running)' -ForegroundColor DarkGray
    } else {
        Start-Sleep -Milliseconds 500
    }

    Initialize-FrontendDeps
    Write-Host '==> Building frontend bundle (pnpm build)...' -ForegroundColor Cyan
    Push-Location $AppDir
    try {
        & pnpm build
        Test-CargoExitOk 'pnpm build'
    } finally {
        Pop-Location
    }

    Write-Host '==> Building daemon + tauri app (release)...' -ForegroundColor Cyan
    & cargo build --release --manifest-path $ManifestPath -p daemon -p rustling-tulip-app --features rustling-tulip-app/custom-protocol
    Test-CargoExitOk 'cargo build --release'

    $appExe    = Get-AppExe
    $daemonExe = Get-DaemonBin -BuildProfile 'release'
    if (-not (Test-Path $appExe))    { throw "Release app exe missing: $appExe" }
    if (-not (Test-Path $daemonExe)) { throw "Release daemon exe missing: $daemonExe" }
}

# ---------------------------------------------------------------------------
# Launchers
# ---------------------------------------------------------------------------

function Invoke-DevLaunch {
    Initialize-FrontendDeps
    Push-Location $AppDir
    try {
        Write-Host '==> Launching app (debug, pnpm tauri dev)...' -ForegroundColor Cyan
        & pnpm tauri dev @Rest
        $exit = $LASTEXITCODE
    } finally {
        Pop-Location
    }
    exit $exit
}

function Invoke-ReleaseLaunch {
    $appExe = Get-AppExe
    if (-not (Test-Path $appExe)) {
        throw "Release app exe missing: $appExe -- run `.\rt.ps1 build -Release` first."
    }
    Write-Host "==> Launching $appExe" -ForegroundColor Cyan
    Start-Process -FilePath $appExe
    Write-Host 'rustling-tulip (release) started.' -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# Subcommand entry points
# ---------------------------------------------------------------------------

function Invoke-Build {
    Assert-Tooling
    if ($Release) { Invoke-ReleaseBuild } else { Invoke-DebugBuild }
}

function Invoke-Launch {
    Assert-Tooling
    if ($Release) { Invoke-ReleaseLaunch } else { Invoke-DevLaunch }
}

function Invoke-Run {
    Assert-Tooling
    if ($Release) {
        Invoke-ReleaseBuild
        Invoke-ReleaseLaunch
        return
    }

    if ($ForceStopDaemon) {
        [void](Stop-DaemonProcesses -Reason 'forced by -ForceStopDaemon')
        $script:DaemonAlreadyStopped = $true
    }

    Invoke-DebugBuild

    # If cargo actually recompiled the daemon (or its deps), the running
    # daemon is now stale -- stop it so the Tauri app spawns a fresh one
    # with the new binary. If it was a no-op, leave the running daemon
    # alone.
    if (-not $script:DaemonAlreadyStopped -and $script:CargoRecompiledDaemon) {
        [void](Stop-DaemonProcesses -Reason 'daemon was rebuilt, restart needed')
    } elseif (-not $script:DaemonAlreadyStopped) {
        Write-Host '==> Daemon up-to-date, leaving running instance alone.' -ForegroundColor DarkGray
    }

    Invoke-DevLaunch
}

function Invoke-Installer {
    Assert-Tooling
    Initialize-FrontendDeps
    Write-Host '==> Building installer bundles (pnpm tauri build)...' -ForegroundColor Cyan
    Push-Location $AppDir
    try {
        & pnpm tauri build @Rest
        Test-CargoExitOk 'pnpm tauri build'
    } finally {
        Pop-Location
    }
    $bundleDir = Join-Path $ScriptDir 'target\release\bundle'
    if (Test-Path $bundleDir) {
        Write-Host "==> Bundles written under $bundleDir" -ForegroundColor Green
    }
}

function Invoke-Stop {
    $stopped = Stop-DaemonProcesses
    if ($stopped) {
        Write-Host '==> Daemon(s) stopped.' -ForegroundColor Green
    } else {
        Write-Host "No $ImageName processes running." -ForegroundColor Yellow
        if (Test-Path $HandshakeFile) {
            Remove-Item $HandshakeFile -Force
            Write-Host 'Removed stale handshake.' -ForegroundColor Yellow
        }
    }
}

function Invoke-Restart {
    [void](Stop-DaemonProcesses -Reason 'restart requested')
    $script:DaemonAlreadyStopped = $true
    Invoke-Run
}

function Invoke-Test {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo test (workspace)...' -ForegroundColor Cyan
    & cargo test --manifest-path $ManifestPath @Rest
    Test-CargoExitOk 'cargo test'
}

function Invoke-Clippy {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo clippy --all-targets --all-features -- -D warnings...' -ForegroundColor Cyan
    & cargo clippy --manifest-path $ManifestPath --all-targets --all-features @Rest -- -D warnings
    Test-CargoExitOk 'cargo clippy'
}

function Invoke-Fmt {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo fmt --all...' -ForegroundColor Cyan
    & cargo fmt --manifest-path $ManifestPath --all @Rest
    Test-CargoExitOk 'cargo fmt'
}

function Invoke-Clean {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo clean...' -ForegroundColor Cyan
    & cargo clean --manifest-path $ManifestPath
    Test-CargoExitOk 'cargo clean'
}

function Show-Help {
    $help = @'
rt.ps1 -- rustling-tulip dev helper

Usage:
  .\rt.ps1 [<command>] [-Release] [-ForceStopDaemon] [-- <extra-args>]

Commands:
  run        Build the daemon then `pnpm tauri dev` (default if omitted).
             With -Release: build the standalone exe and launch it detached.
  build      Build only -- no app launch. Debug by default; -Release for
             the standalone-app release artifacts.
  launch     Launch only -- assumes binaries are already built.
  installer  `pnpm tauri build` -- produces MSI + NSIS installer bundles
             under target\release\bundle\.
  stop       Kill any running daemon and remove the stale handshake.
  restart    stop + run. Fresh dev loop after the daemon misbehaves.
  test       `cargo test` across the workspace.
  clippy     `cargo clippy --all-targets --all-features -- -D warnings`.
  fmt        `cargo fmt --all`.
  clean      `cargo clean`.
  help       This message.

Flags:
  -Release           Use the release profile (build/launch/run/restart).
  -ForceStopDaemon   Stop the daemon up-front (run/restart) even when cargo
                     says no rebuild is needed.

Examples:
  .\rt.ps1                   # build + launch dev
  .\rt.ps1 build -Release    # release artifacts, no launch
  .\rt.ps1 launch            # pnpm tauri dev, no build
  .\rt.ps1 installer         # MSI + NSIS bundles
  .\rt.ps1 restart           # kill daemon + relaunch dev
  .\rt.ps1 clippy
'@
    Write-Host $help
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

$effective = if ([string]::IsNullOrEmpty($Command)) { 'run' } else { $Command }

switch ($effective) {
    'run'       { Invoke-Run }
    'build'     { Invoke-Build }
    'launch'    { Invoke-Launch }
    'installer' { Invoke-Installer }
    'stop'      { Invoke-Stop }
    'restart'   { Invoke-Restart }
    'test'      { Invoke-Test }
    'clippy'    { Invoke-Clippy }
    'fmt'       { Invoke-Fmt }
    'clean'     { Invoke-Clean }
    'help'      { Show-Help }
    default     { throw "Unknown command: $effective" }
}
