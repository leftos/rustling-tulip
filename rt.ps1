#Requires -Version 7.0
<#
.SYNOPSIS
    Rustling-tulip dev helper -- build, run the native client, test, lint,
    format, clean, and manage the daemon process.

.DESCRIPTION
    Subcommands:

      launch     Build the daemon and tracer, then run the native (GPUI)
                 client via `cargo run -p rustling-tulip-native`. This is
                 the default when no subcommand is given; same as `native`.
      build      Build only -- the daemon, tracer and native client. Debug
                 by default; -Release for the release profile. Then sweeps
                 build output older than 14 days out of target/ (needs
                 cargo-sweep; a build without it skips the sweep).
      setup      Install/check Windows build prerequisites via winget:
                 Git, Node.js LTS (fake-claude needs it), Rust/rustup,
                 Visual Studio C++ Build Tools and cargo-sweep.
      stop       Kill any running daemon and tracer process(es) and clean
                 the stale handshake file. No-op when nothing is running.
      restart    Build the daemon and tracer, stop the running daemon
                 (tracers survive, so sessions reattach), then run the
                 native client, unless one is already running (it
                 reconnects to the new daemon). Use it after changing the
                 daemon, tracer or protocol: `launch` and `native` reuse a
                 running compatible daemon even from an older build.
      test       Run `cargo test` across the workspace.
      clippy     Run the strict workspace clippy pass
                 (`--all-targets --all-features -- -D warnings`).
      fmt        Run `cargo fmt --all`.
      clean      Run `cargo clean`.
      native     Build the daemon and tracer, then run the native client.
                 Extra arguments go to the client (an optional session id
                 to focus, or to place in the active tab).
      native-e2e Build the daemon and tracer, then run the native client's
                 end-to-end specs (`--test e2e_live --test e2e_recover
                 -- --ignored`) against
                 a real daemon isolated under `.tmp\native-e2e\`. The
                 fake-claude spec needs `node` on PATH.
      native-smoke
                 Build the daemon and tracer, then run the native client's
                 OS smoke specs (`--test smoke_window -- --ignored`): the
                 real client binary in a cloaked window that never
                 takes focus; checks it connects and that posted keys
                 reach the shell.
      native-shot
                 Write a PNG of the native client's window showing one view
                 (`main`, `main-compact`, `source-control`, `diff`, `settings`
                 or `spawn`; default `main`) over a fake daemon with fixed
                 content, via
                 `cargo run --example shot`. No daemon starts; the window
                 stays cloaked and never takes focus. Writes
                 `.tmp\shots\<view>.png` unless a second argument or -Out
                 gives another path, and prints the PNG's path.
      help       Print the subcommand summary.

.PARAMETER Command
    The subcommand to run (positional). When omitted, defaults to `launch`.

.PARAMETER Release
    Applies to `build`, `launch`, `restart`, `native`, `native-e2e`,
    `native-smoke` and `native-shot`. Selects the release profile instead
    of debug.

.PARAMETER Out
    Applies to `native-shot`: the PNG to write, instead of the second
    positional argument and `.tmp\shots\<view>.png`.

.EXAMPLE
    .\rt.ps1                       # = .\rt.ps1 launch
    .\rt.ps1 build -Release
    .\rt.ps1 launch -Release
    .\rt.ps1 setup
    .\rt.ps1 stop
    .\rt.ps1 restart
    .\rt.ps1 clippy
#>
[CmdletBinding()]
# Write-Host is intentional: this is an interactive dev script and the colored
# status lines are how the user sees progress.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Interactive dev script; colored status to console is the UX.')]
# $Release and $Rest are read by sub-functions via the script scope;
# PSScriptAnalyzer's parameter-usage check doesn't trace that.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '',
    Justification = 'Top-level params consumed by sub-functions via $script: scope.')]
param(
    [Parameter(Position = 0)]
    [ValidateSet('', 'build', 'launch', 'setup', 'stop', 'restart', 'test', 'clippy', 'fmt', 'clean', 'native', 'native-e2e', 'native-smoke', 'native-shot', 'help')]
    [string]$Command = '',

    [switch]$Release,

    # The PNG `native-shot` writes.
    [string]$Out,

    # Extra arguments forwarded to the underlying tool (e.g. `cargo test --
    # mytest`). Only meaningful for `launch`, `restart`, `test`, `clippy`,
    # `fmt`, `native`, `native-shot` (the view).
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Platform
# ---------------------------------------------------------------------------

# Resolve the daemon's config directory the same way the daemon does
# (crates/daemon/src/paths.rs -> directories::ProjectDirs::config_dir),
# honoring the RUSTLING_TULIP_CONFIG_DIR override the daemon also respects.
# daemon.json (the handshake file the stop/restart commands read) lives
# directly in this directory.
function Get-ConfigDir {
    if ($env:RUSTLING_TULIP_CONFIG_DIR) {
        return $env:RUSTLING_TULIP_CONFIG_DIR
    }
    if ($IsWindows) {
        return Join-Path -Path $env:APPDATA -ChildPath 'leftos' -AdditionalChildPath 'rustling-tulip', 'config'
    }
    if ($IsMacOS) {
        return Join-Path -Path $HOME -ChildPath 'Library' -AdditionalChildPath 'Application Support', 'dev.leftos.rustling-tulip'
    }
    # Linux / other Unix: XDG config home (app name only).
    $xdg = if ($env:XDG_CONFIG_HOME) { $env:XDG_CONFIG_HOME } else { Join-Path $HOME '.config' }
    return Join-Path $xdg 'rustling-tulip'
}

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

$ScriptDir       = Split-Path -Parent $MyInvocation.MyCommand.Path
$ManifestPath    = Join-Path $ScriptDir 'Cargo.toml'
$ImageName       = 'rustling-tulipd'
$TracerImageName = 'rt-tracer'
$HandshakeFile   = Join-Path (Get-ConfigDir) 'daemon.json'

$script:SetupRestartNeeded = $false

# `build` sweeps artifacts older than this out of target/ afterwards
# (cargo-sweep); build output had grown to fill the drive.
$SweepAgeDays = 14

# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

function Test-Tool {
    param([string]$Name, [string]$InstallHint)
    if (-not (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "$Name not found on PATH. $InstallHint"
    }
}

function Test-CommandAvailable {
    param([string]$Name)
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Update-ProcessPath {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute(
        'PSUseShouldProcessForStateChangingFunctions',
        '',
        Justification = 'Internal helper that refreshes the in-process PATH from the registry; no -WhatIf surface needed.'
    )]
    param()

    $seen = [System.Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $parts = [System.Collections.Generic.List[string]]::new()
    foreach ($scope in @('Process', 'Machine', 'User')) {
        $value = [Environment]::GetEnvironmentVariable('Path', $scope)
        if (-not $value) { continue }
        foreach ($part in ($value -split ';')) {
            $trimmed = $part.Trim()
            if ($trimmed -and $seen.Add($trimmed)) {
                [void]$parts.Add($trimmed)
            }
        }
    }
    $env:Path = $parts -join ';'
}

function Invoke-WingetInstall {
    [CmdletBinding()]
    param(
        [string]$Id,
        [string]$Name,
        [string[]]$ExtraArgs = @()
    )

    Test-Tool 'winget' 'Install App Installer from the Microsoft Store, then rerun `.\rt.ps1 setup`.'

    Write-Host "==> Installing $Name..." -ForegroundColor Cyan
    $wingetArgs = @(
        'install',
        '--id', $Id,
        '--exact',
        '--source', 'winget',
        '--accept-package-agreements',
        '--accept-source-agreements',
        '--disable-interactivity',
        '--silent'
    )
    $wingetArgs += $ExtraArgs

    & winget @wingetArgs
    if ($LASTEXITCODE -eq 3010 -or $LASTEXITCODE -eq 1641) {
        $script:SetupRestartNeeded = $true
        Write-Host "    $Name installed; Windows reported a restart is needed." -ForegroundColor Yellow
    } elseif ($LASTEXITCODE -ne 0) {
        throw "$Name install failed (exit $LASTEXITCODE)"
    }

    Update-ProcessPath
}

function Get-VsWherePath {
    $cmd = Get-Command 'vswhere.exe' -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }

    $defaultPath = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $defaultPath) { return $defaultPath }
    return $null
}

function Get-MsvcInstallPath {
    $vswhere = Get-VsWherePath
    if (-not $vswhere) { return $null }

    $vsWhereArgs = @(
        '-latest',
        '-products', '*',
        '-requires', 'Microsoft.VisualStudio.Component.VC.Tools.x86.x64',
        '-property', 'installationPath'
    )
    $installPath = & $vswhere @vsWhereArgs
    if ($LASTEXITCODE -ne 0 -or -not $installPath) { return $null }
    return ($installPath | Select-Object -First 1).Trim()
}

function Test-MsvcBuildTools {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute(
        'PSUseSingularNouns',
        '',
        Justification = '"Build Tools" is the proper product name for Visual Studio Build Tools.'
    )]
    param()
    return $null -ne (Get-MsvcInstallPath)
}

function Initialize-MsvcEnvironment {
    [CmdletBinding()]
    [OutputType([bool])]
    param([switch]$Quiet)

    if (-not $IsWindows) { return $true }
    if (Test-CommandAvailable 'link.exe') { return $true }

    $installPath = Get-MsvcInstallPath
    if (-not $installPath) { return $false }

    $vsDevCmd = Join-Path $installPath 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path $vsDevCmd)) { return $false }

    if (-not $Quiet) {
        Write-Host '==> Loading MSVC build environment...' -ForegroundColor Cyan
    }

    # VsDevCmd.bat calls vswhere.exe by bare name, so its folder must be on PATH.
    $vsWhereDir = Split-Path (Get-VsWherePath)
    $cmd = "set `"PATH=$vsWhereDir;%PATH%`" && `"$vsDevCmd`" -arch=x64 -host_arch=x64 >nul && set"
    $lines = & cmd.exe /d /s /c $cmd
    if ($LASTEXITCODE -ne 0) { return $false }

    foreach ($line in $lines) {
        if ($line -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($matches[1], $matches[2], 'Process')
        }
    }

    return (Test-CommandAvailable 'link.exe')
}

function Assert-MsvcLinker {
    if (-not $IsWindows) { return }
    if (-not (Initialize-MsvcEnvironment -Quiet)) {
        throw 'MSVC linker link.exe not found. Run `.\rt.ps1 setup` to install Visual Studio C++ Build Tools, then open a new PowerShell if setup requested it.'
    }
}

function Assert-Tooling {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Asserts presence of multiple tools; plural noun is accurate.')]
    param()
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Assert-MsvcLinker
}

function Test-CargoExitOk {
    param([string]$What)
    if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

function Stop-DaemonProcesses {
    [CmdletBinding()]
    [OutputType([bool])]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Stops zero-or-more processes; plural noun is accurate.')]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'Dev script: the subcommand itself is the user gesture; no extra prompt needed.')]
    # Stops the specified process names. Defaults to daemon-only so
    # the routine "rebuilt the daemon, restart it" path preserves
    # Phase C.3 session reattach -- the surviving tracers keep the
    # claude/codex/shell children alive and the freshly-launched
    # daemon picks them back up. Pass an explicit -Names list when
    # the caller wants a full teardown (`rt.ps1 stop`).
    param(
        [string]$Reason,
        [string[]]$Names = @($ImageName),
        [switch]$AllowRespawn
    )
    # Append `*` so cached binary copies (`rustling-tulipd-<hash>.exe`,
    # `rt-tracer-<hash>.exe`) are caught alongside the original templates.
    # Clients and the daemon spawn from the content-addressed binary cache
    # (see crates/daemon/src/binary_cache.rs), so the running ProcessName
    # has a hash suffix and an exact-match `-Name rustling-tulipd` misses it.
    $targetNames = @($Names | ForEach-Object { "$_*" })
    $processes = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
    if ($processes.Count -eq 0) {
        return $false
    }
    $byName = $processes | Group-Object -Property ProcessName |
        ForEach-Object { "$($_.Count) $($_.Name)" }
    $summary = $byName -join ' + '
    $msg = if ($Reason) { "==> Stopping $summary process(es) -- $Reason..." }
           else        { "==> Stopping $summary process(es)..." }
    Write-Host $msg -ForegroundColor Yellow
    $stoppedIds = @($processes | ForEach-Object { $_.Id })
    foreach ($p in $processes) {
        Write-Host "    killing PID $($p.Id) ($($p.ProcessName))"
        Stop-Process -Id $p.Id -Force -ErrorAction Continue
    }
    # Give the OS up to 2s for handles to release.
    $remaining = @()
    for ($i = 0; $i -lt 20; $i++) {
        if ($AllowRespawn) {
            $remaining = @(Get-Process -Id $stoppedIds -ErrorAction SilentlyContinue)
        } else {
            $remaining = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
        }
        if ($remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($AllowRespawn) {
        $remaining = @(Get-Process -Id $stoppedIds -ErrorAction SilentlyContinue)
    } else {
        $remaining = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
    }
    if ($remaining.Count -gt 0) {
        $names = ($remaining | ForEach-Object { "$($_.ProcessName)#$($_.Id)" }) -join ', '
        throw "Failed to stop $($remaining.Count) process(es) within 2s: $names"
    }
    if (Test-Path $HandshakeFile) {
        $current = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
        if (-not $AllowRespawn -or $current.Count -eq 0) {
            Remove-Item $HandshakeFile -Force
        }
    }
    return $true
}

function Install-CommandDependency {
    [CmdletBinding()]
    param(
        [string]$CommandName,
        [string]$WingetId,
        [string]$DisplayName
    )

    if (Test-CommandAvailable $CommandName) {
        Write-Host "==> $DisplayName already available." -ForegroundColor DarkGray
        return
    }
    Invoke-WingetInstall -Id $WingetId -Name $DisplayName
}

function Install-CargoSweep {
    # Only cargo subcommand in the prerequisites -- not a winget package,
    # so it gets its own installer rather than Install-CommandDependency.
    [CmdletBinding()]
    param()

    if (Test-CommandAvailable 'cargo-sweep') {
        Write-Host '==> cargo-sweep already available.' -ForegroundColor DarkGray
        return
    }

    Write-Host '==> Installing cargo-sweep (cargo install cargo-sweep --locked)...' -ForegroundColor Cyan
    & cargo install cargo-sweep --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo install cargo-sweep failed (exit $LASTEXITCODE)"
    }
}

function Install-MsvcBuildTools {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute(
        'PSUseSingularNouns',
        '',
        Justification = '"Build Tools" is the proper product name for Visual Studio Build Tools.'
    )]
    param()
    if (Test-MsvcBuildTools) {
        Write-Host '==> Visual Studio C++ Build Tools already available.' -ForegroundColor DarkGray
        return
    }

    $override = '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
    Invoke-WingetInstall `
        -Id 'Microsoft.VisualStudio.2022.BuildTools' `
        -Name 'Visual Studio 2022 Build Tools with Desktop development with C++' `
        -ExtraArgs @('--override', $override)

    if (-not (Test-MsvcBuildTools) -and $script:SetupRestartNeeded) {
        throw 'Visual Studio Build Tools installed, but Windows requested a restart before the C++ toolchain can be detected. Restart, then rerun `.\rt.ps1 setup`.'
    }
    if (-not (Test-MsvcBuildTools)) {
        throw 'Visual Studio Build Tools installed, but the C++ toolchain was not detected. Open Visual Studio Installer and add "Desktop development with C++", then rerun `.\rt.ps1 setup`.'
    }
}

function Initialize-RustToolchain {
    Test-Tool 'rustup' 'Run `.\rt.ps1 setup` to install Rust via rustup.'

    Write-Host '==> Selecting the stable MSVC Rust toolchain...' -ForegroundColor Cyan
    & rustup default stable-msvc
    Test-CargoExitOk 'rustup default stable-msvc'

    Write-Host '==> Ensuring rustfmt and clippy are installed...' -ForegroundColor Cyan
    & rustup component add rustfmt clippy
    Test-CargoExitOk 'rustup component add rustfmt clippy'
}

function Invoke-Setup {
    if (-not $IsWindows) {
        throw 'The setup command currently supports Windows only. Install Rust and Node.js manually on this OS.'
    }

    Test-Tool 'winget' 'Install App Installer from the Microsoft Store, then rerun `.\rt.ps1 setup`.'

    Write-Host '==> Installing/checking Windows build prerequisites...' -ForegroundColor Cyan
    Install-CommandDependency -CommandName 'git' -WingetId 'Git.Git' -DisplayName 'Git'
    Install-CommandDependency -CommandName 'node' -WingetId 'OpenJS.NodeJS.LTS' -DisplayName 'Node.js LTS'

    if ((Test-CommandAvailable 'cargo') -and (Test-CommandAvailable 'rustup')) {
        Write-Host '==> Rust/rustup already available.' -ForegroundColor DarkGray
    } else {
        Invoke-WingetInstall -Id 'Rustlang.Rustup' -Name 'Rustup'
    }
    Update-ProcessPath

    Install-MsvcBuildTools
    if (-not (Initialize-MsvcEnvironment)) {
        throw 'Could not load the MSVC environment after installing Build Tools. Open a new PowerShell and rerun `.\rt.ps1 setup`.'
    }

    Initialize-RustToolchain

    Assert-Tooling
    Install-CargoSweep
    if ($script:SetupRestartNeeded) {
        Write-Host '==> Setup complete. Restart Windows before building.' -ForegroundColor Yellow
    } else {
        Write-Host '==> Setup complete.' -ForegroundColor Green
    }
}

# ---------------------------------------------------------------------------
# Subcommand entry points
# ---------------------------------------------------------------------------

function Invoke-Build {
    Assert-Tooling
    $buildArgs = @('build', '--manifest-path', $ManifestPath, '-p', 'daemon', '-p', 'tracer', '-p', 'rustling-tulip-native')
    if ($Release) { $buildArgs += '--release' }
    $modeLabel = if ($Release) { 'release' } else { 'debug' }
    Write-Host "==> Building daemon + tracer + native client ($modeLabel)..." -ForegroundColor Cyan
    & cargo @buildArgs
    Test-CargoExitOk 'cargo build -p daemon -p tracer -p rustling-tulip-native'
    Invoke-BuildSweep
}

# Reclaims the disk the artifacts of earlier builds hold; a full target/
# had once filled the drive. Runs after a successful build, never fails
# it: a missing cargo-sweep only prints how to install it.
function Invoke-BuildSweep {
    [CmdletBinding()]
    param()

    $targetDir = Join-Path $ScriptDir 'target'
    if (-not (Test-Path $targetDir)) { return }

    if (-not (Test-CommandAvailable 'cargo-sweep')) {
        Write-Host '==> cargo-sweep not installed; skipping the build-output sweep. Run `.\rt.ps1 setup` to install it.' -ForegroundColor Yellow
        return
    }

    Write-Host "==> Sweeping build output older than $SweepAgeDays days..." -ForegroundColor Cyan
    & cargo sweep --time $SweepAgeDays $ScriptDir
    if ($LASTEXITCODE -ne 0) {
        Write-Host "==> cargo sweep failed (exit $LASTEXITCODE); continuing." -ForegroundColor Yellow
    }
}

function Invoke-Stop {
    # Explicit user `stop` gesture -- tear down sessions too. (Restart
    # path leaves tracers alive so Phase C.3 reattach can resume
    # sessions transparently.)
    $stopped = Stop-DaemonProcesses -Names @($ImageName, $TracerImageName)
    if ($stopped) {
        Write-Host '==> Daemon + tracer process(es) stopped.' -ForegroundColor Green
    } else {
        Write-Host "No $ImageName or $TracerImageName processes running." -ForegroundColor Yellow
        if (Test-Path $HandshakeFile) {
            Remove-Item $HandshakeFile -Force
            Write-Host 'Removed stale handshake.' -ForegroundColor Yellow
        }
    }
}

# Builds first so the daemon is down only for the stop-and-respawn, not
# for the compile. Only the daemon is stopped: the tracers keep their
# sessions alive and the daemon the client spawns reattaches them.
# -AllowRespawn because a native client already open may spawn a fresh
# daemon before the stop's wait ends. The client has no single-instance
# guard, so an open one is left to reconnect rather than joined by a second
# window sharing its layout.
function Invoke-Restart {
    Build-DaemonAndTracer
    [void](Stop-DaemonProcesses -Reason 'restart requested' -AllowRespawn)
    $running = @(Get-Process -Name 'rustling-tulip-native*' -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0) {
        Write-Host '==> Native client already running; it will reconnect to the new daemon.' -ForegroundColor DarkGray
        return
    }
    Start-NativeClient
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

function Build-DaemonAndTracer {
    Assert-Tooling
    $buildArgs = @('build', '--manifest-path', $ManifestPath, '-p', 'daemon', '-p', 'tracer')
    if ($Release) { $buildArgs += '--release' }
    Write-Host '==> Building daemon + tracer...' -ForegroundColor Cyan
    & cargo @buildArgs
    Test-CargoExitOk 'cargo build -p daemon -p tracer'
}

function Start-NativeClient {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'Dev script: the calling subcommand is the user gesture.')]
    param()
    $cargoArgs = @('run', '--manifest-path', $ManifestPath, '-p', 'rustling-tulip-native')
    if ($Release) { $cargoArgs += '--release' }
    # Everything after `--` goes to the client, not cargo (the session id).
    if ($Rest) { $cargoArgs += @('--') + $Rest }
    $modeLabel = if ($Release) { 'release' } else { 'debug' }
    Write-Host "==> Running native client ($modeLabel)..." -ForegroundColor Cyan
    & cargo @cargoArgs
    Test-CargoExitOk 'cargo run -p rustling-tulip-native'
}

function Invoke-Native {
    # The client spawns the daemon when none is running, from the daemon and
    # tracer binaries in the same profile's target dir; build them first.
    Build-DaemonAndTracer
    Start-NativeClient
}

# Runs one of the native client's ignored spec files. The specs start their
# own daemon from the binaries beside the test binary, so the daemon and
# tracer are built first in the same profile. One thread: each spec owns a
# daemon and, for the smoke specs, a client window.
function Invoke-NativeSpecFile([string[]]$TestFiles) {
    Build-DaemonAndTracer
    $testArgs = @('test', '--manifest-path', $ManifestPath, '-p', 'rustling-tulip-native')
    foreach ($file in $TestFiles) { $testArgs += @('--test', $file) }
    if ($Release) { $testArgs += '--release' }
    $testArgs += @('--', '--ignored', '--test-threads=1')
    $names = $TestFiles -join ', '
    Write-Host "==> Running native specs: $names..." -ForegroundColor Cyan
    & cargo @testArgs
    Test-CargoExitOk "cargo test -p rustling-tulip-native ($names)"
}

function Invoke-NativeE2e { Invoke-NativeSpecFile @('e2e_live', 'e2e_recover') }

function Invoke-NativeSmoke { Invoke-NativeSpecFile 'smoke_window' }

# Writes a PNG of the native client's window showing one view, over a fake
# daemon with fixed content (apps/native/examples/shot.rs). No daemon starts,
# so none is built; the window stays cloaked and never takes focus. The first
# extra argument is the view; the rest go to the example.
function Invoke-NativeShot {
    Assert-Tooling
    $view = if ($Rest) { $Rest[0] } else { 'main' }
    if ($Rest -and $Rest.Count -gt 2) {
        throw "native-shot takes at most two positional arguments (the view and the output PNG), got '$($Rest[2..($Rest.Count - 1)] -join ' ')'; pass the output path with -Out <path>."
    }
    $positional = if ($Rest -and $Rest.Count -gt 1) { $Rest[1] } else { '' }
    $outPath = if ($Out) {
        [IO.Path]::GetFullPath($Out, (Get-Location).Path)
    } elseif ($positional) {
        [IO.Path]::GetFullPath($positional, (Get-Location).Path)
    } else {
        Join-Path $ScriptDir '.tmp' 'shots' "$view.png"
    }
    $cargoArgs = @('run', '--manifest-path', $ManifestPath, '-p', 'rustling-tulip-native', '--example', 'shot')
    if ($Release) { $cargoArgs += '--release' }
    $cargoArgs += @('--', $view, $outPath)
    Write-Host "==> Taking a shot of the native client's '$view' view..." -ForegroundColor Cyan
    & cargo @cargoArgs
    Test-CargoExitOk 'cargo run -p rustling-tulip-native --example shot'
    Write-Host $outPath
}

function Show-Help {
    $help = @'
rt.ps1 -- rustling-tulip dev helper

Usage:
  .\rt.ps1 [<command>] [-Release] [-- <extra>]

Commands:
  launch     Build daemon + tracer, then run the native client (default
             if omitted; same as `native`).
  build      Build only: daemon, tracer and native client. Then sweeps
             build output older than 14 days out of target/ (needs
             cargo-sweep; a build without it skips the sweep).
  setup      Install/check Windows build prerequisites via winget (Git,
             Node.js, Rust, C++ Build Tools, cargo-sweep). Run once
             after cloning.
  stop       Kill any running daemon and tracers; remove the stale
             handshake.
  restart    Build daemon + tracer, stop the running daemon (sessions
             survive in their tracers), then run the native client (an
             open client is left to reconnect instead).
  test       `cargo test` across the workspace.
  clippy     `cargo clippy --all-targets --all-features -- -D warnings`.
  fmt        `cargo fmt --all`.
  clean      `cargo clean`.
  native     `cargo build -p daemon -p tracer`, then `cargo run -p
             rustling-tulip-native`. Extra args are passed on (an
             optional session id to focus, or to place in the active tab).
  native-e2e Build daemon + tracer, then run the native client's
             end-to-end specs against an isolated daemon (needs `node`).
  native-smoke
             Build daemon + tracer, then run the native client's OS smoke
             specs in a cloaked window that never takes focus; checks it
             connects and that posted keys reach the shell.
  native-shot [<view>] [<out.png>] [-Out <path>]
             Write a PNG of the native client's window showing <view>
             (main, main-compact, source-control, diff, settings, spawn;
             default main)
             over a fake daemon with fixed content, then print its path.
             <out.png> is the PNG to write; -Out says the same thing.
             Default: .tmp\shots\<view>.png. No daemon starts; the window
             stays cloaked and never takes focus.
  help       This message.

`launch` and `native` reuse a running daemon whose protocol matches, even
one from an older build: use `restart` after changing the daemon, tracer
or protocol.

Flags:
  -Release           Use the release profile (build, launch, restart,
                     native, native-e2e, native-smoke, native-shot).
  -Out <path>        The PNG native-shot writes.

Examples:
  .\rt.ps1                       # build + run the native client
  .\rt.ps1 build -Release        # release binaries, no launch
  .\rt.ps1 setup                 # install/check build prerequisites
  .\rt.ps1 launch -Release       # build + run the release client
  .\rt.ps1 restart               # rebuild, bounce the daemon, run client
  .\rt.ps1 clippy
'@
    Write-Host $help
}

# ---------------------------------------------------------------------------
# Dispatch
# ---------------------------------------------------------------------------

$effective = if ([string]::IsNullOrEmpty($Command)) { 'launch' } else { $Command }

switch ($effective) {
    'build'     { Invoke-Build }
    'launch'    { Invoke-Native }
    'setup'     { Invoke-Setup }
    'stop'      { Invoke-Stop }
    'restart'   { Invoke-Restart }
    'test'      { Invoke-Test }
    'clippy'    { Invoke-Clippy }
    'fmt'       { Invoke-Fmt }
    'clean'     { Invoke-Clean }
    'native'    { Invoke-Native }
    'native-e2e'   { Invoke-NativeE2e }
    'native-smoke' { Invoke-NativeSmoke }
    'native-shot'  { Invoke-NativeShot }
    'help'      { Show-Help }
    default     { throw "Unknown command: $effective" }
}
