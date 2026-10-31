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
                 Git, Node.js LTS (the agents' fake CLIs need it), Rust/rustup,
                 Visual Studio C++ Build Tools, cargo-sweep and
                 cargo-nextest.
      stop       Kill any running daemon and tracer process(es) and clean
                 the stale handshake file. No-op when nothing is running.
      restart    Build the daemon and tracer, stop the running daemon
                 (tracers survive, so sessions reattach), then run the
                 native client, unless one is already running (it
                 reconnects to the new daemon). Use it after changing the
                 daemon, tracer or protocol: `launch` and `native` reuse a
                 running compatible daemon even from an older build.
      test       Run the workspace's tests with cargo-nextest
                 (`cargo nextest run --cargo-profile test-release`).
                 -Reached runs only the tests the change since -Base
                 reaches, mapped through tools/test-map.json (a central or
                 unmapped file runs the full suite); -PrintOnly prints the
                 selection without running it; -Validate checks the map.
      clippy     Run the agent-name seam check (tools/check-agent-names.ps1),
                 then the strict workspace clippy pass
                 (`--all-targets --all-features -- -D warnings`).
      fmt        Run `cargo fmt --all`.
      clean      Run `cargo clean`.
      native     Build the daemon and tracer, then run the native client.
                 Extra arguments go to the client (an optional session id
                 to focus, or to place in the active tab).
      native-e2e Build the daemon (with the `e2e-loopback-peers` feature)
                 and tracer, then run the native client's end-to-end specs
                 (`e2e_live`, `e2e_recover`, `e2e_remote`, `e2e_ssh`,
                 `e2e_multi_host`, under
                 nextest `--run-ignored only`, test-release profile) against
                 a real daemon isolated under `.tmp\native-e2e\`. The agents'
                 fake CLIs (crates\agents\claude\fake), fake-tailscale and
                 fake-ssh need `node` on PATH. Refuses
                 -Release: the test-only daemon stays in test-release.
      native-smoke
                 Build the daemon and tracer, then run the native client's
                 OS smoke specs (`smoke_window`, `--run-ignored only`): the
                 real client binary in a cloaked window that never
                 takes focus; checks it connects and that posted keys
                 reach the shell.
      native-shot
                 Write a PNG of the native client's window showing one view
                 (`main`, `main-compact`, `source-control`, `diff`, `settings`,
                 `settings-claude`, `spawn`, `viewer`, `viewer-md`, `dashboard`, `connections`, `connections-ssh`, `menu`, `update`, `chat`, `summary`, `themes`, `themes-edit`, `multi-host`, `spawn-hosts` or `settings-hosts`; default `main`) over a fake daemon with fixed
                 content, via
                 `cargo run --example shot`. No daemon starts; the window
                 stays cloaked and never takes focus. Writes
                 `.tmp\shots\<view>.png` unless a second argument or -Out
                 gives another path, and prints the PNG's path.
      installer  Build the release binaries (daemon, tracer, native
                 client), then package them into an NSIS setup exe under
                 `target\installer\` and print its path. Always release;
                 needs NSIS 3 (`winget install NSIS.NSIS`).
      help       Print the subcommand summary.

.PARAMETER Command
    The subcommand to run (positional). When omitted, defaults to `launch`.

.PARAMETER Release
    Applies to `build`, `launch`, `restart`, `native`, `native-smoke` and
    `native-shot`. Selects the release profile instead of debug.
    `native-e2e` refuses it: its daemon is a test-only build.

.PARAMETER Out
    Applies to `native-shot`: the PNG to write, instead of the second
    positional argument and `.tmp\shots\<view>.png`.

.PARAMETER Reached
    Applies to `test`: run only the tests the files changed since -Base
    reach (`git diff --name-only <base>` plus untracked files), mapped
    through tools/test-map.json.

.PARAMETER Base
    Applies to `test -Reached`: the ref to diff against. Default: the
    branch's `branch.<name>.base` git config, else main's merge-base with
    HEAD.

.PARAMETER PrintOnly
    Applies to `test`: print the selection without running any test.

.PARAMETER Validate
    Applies to `test`: check that tools/test-map.json has a row (or a
    central entry) for every native source and test file, and that its
    filters name real nextest binaries and modules.

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
    [ValidateSet('', 'build', 'launch', 'setup', 'stop', 'restart', 'test', 'clippy', 'fmt', 'clean', 'native', 'native-e2e', 'native-smoke', 'native-shot', 'installer', 'help')]
    [string]$Command = '',

    [switch]$Release,

    # The PNG `native-shot` writes.
    [string]$Out,

    # `test`: run only the tests the change since -Base reaches (tools/test-map.json).
    [switch]$Reached,

    # `test -Reached`: the ref to diff against.
    [string]$Base,

    # `test`: print the selection without running it.
    [switch]$PrintOnly,

    # `test`: check tools/test-map.json covers every native file and names real binaries.
    [switch]$Validate,

    # `native-e2e` internal: the re-entry that builds and runs only the sealed-secret
    # spec on this PC (.gate/local.toml keeps it here; it needs Credential Manager).
    [switch]$SealedSecretOnly,

    # Extra arguments forwarded to the underlying tool (e.g. `cargo nextest
    # run` filters). Only meaningful for `launch`, `restart`, `test`, `clippy`,
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
$TestMapPath     = Join-Path $ScriptDir 'tools/test-map.json'

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

# True inside a gate's command: a local gate sets GATE_SLOT_HELD, a routed run on a builder sets GATE_BOX_JOB.
function Test-UnderGate {
    [OutputType([bool])]
    param()
    return [bool]($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB)
}

# Runs a command through tools/gate.ps1, or directly when already under a gate
# (Test-UnderGate): a nested gate call would queue behind the slot this run already holds.
function Invoke-Gated([string]$Slot, [string]$Name, [string[]]$Command) {
    if (Test-UnderGate) {
        & $Command[0] @($Command | Select-Object -Skip 1)
        return
    }
    $tmp = Join-Path $ScriptDir '.tmp'
    if (-not (Test-Path $tmp)) { New-Item -ItemType Directory -Force -Path $tmp | Out-Null }
    $timeout = if ($Slot -eq 'heavy') { 3600 } else { 900 }
    $log = Join-Path $tmp "rt-$Name.log"
    & pwsh -NoProfile -File (Join-Path $ScriptDir 'tools/gate.ps1') run --log $log --timeout $timeout --slot $Slot -- @Command
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

function Install-CargoSubcommand {
    # Cargo subcommands are not winget packages, so they get their own
    # installer rather than Install-CommandDependency.
    [CmdletBinding()]
    param([string]$Name)

    if (Test-CommandAvailable $Name) {
        Write-Host "==> $Name already available." -ForegroundColor DarkGray
        return
    }

    Write-Host "==> Installing $Name (cargo install $Name --locked)..." -ForegroundColor Cyan
    & cargo install $Name --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo install $Name failed (exit $LASTEXITCODE)"
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
    Install-CargoSubcommand 'cargo-sweep'
    Install-CargoSubcommand 'cargo-nextest'
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
    Invoke-Gated -Slot heavy -Name build -Command (@('cargo') + $buildArgs)
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
    Invoke-Gated -Slot light -Name sweep -Command @('cargo', 'sweep', '--time', $SweepAgeDays, $ScriptDir)
    if ($LASTEXITCODE -ne 0) {
        Write-Host "==> cargo sweep failed (exit $LASTEXITCODE); continuing." -ForegroundColor Yellow
    }
}

# Reads `version` from the `[workspace.package]` table of the workspace manifest.
function Get-WorkspaceVersion {
    $manifest = Get-Content -Raw -Path $ManifestPath
    $table = [regex]::Match($manifest, '(?ms)^\[workspace\.package\]\s*$(.*?)(?=^\[|\z)')
    if (-not $table.Success) { throw "No [workspace.package] table in $ManifestPath." }
    $version = [regex]::Match($table.Groups[1].Value, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    if (-not $version) { throw "No version line in [workspace.package] in $ManifestPath." }
    return $version
}

# NSIS's makensis.exe: the default install location, then PATH. $null when absent.
function Get-MakensisPath {
    $roots = @(${env:ProgramFiles(x86)}, $env:ProgramFiles) | Where-Object { $_ }
    foreach ($root in $roots) {
        $candidate = Join-Path $root 'NSIS\makensis.exe'
        if (Test-Path $candidate) { return $candidate }
    }
    $onPath = Get-Command makensis -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }
    return $null
}

# Builds the release binaries and packages them into the NSIS setup exe.
# Always release: an installer ships release binaries whatever -Release says.
function Invoke-Installer {
    Assert-Tooling

    $makensis = Get-MakensisPath
    if (-not $makensis) {
        throw 'NSIS 3 is required to build the installer: winget install NSIS.NSIS.'
    }

    $version = Get-WorkspaceVersion
    # VIProductVersion takes four numeric parts, so the pre-release suffix
    # (`0.1.0-alpha`) stays out of it and only names the exe and DisplayVersion.
    $numericVersion = ($version -split '-', 2)[0]
    $binDir = Join-Path $ScriptDir 'target/release'
    $installerDir = Join-Path $ScriptDir 'target/installer'
    $outFile = Join-Path $installerDir "rustling-tulip-$version-setup.exe"
    $nsiScript = Join-Path $ScriptDir 'tools/installer/rustling-tulip.nsi'

    Write-Host "==> Building daemon + tracer + native client (release, $version)..." -ForegroundColor Cyan
    $releaseArgs = @(
        'cargo', 'build', '--manifest-path', $ManifestPath, '--release',
        '-p', 'daemon', '-p', 'tracer', '-p', 'rustling-tulip-native'
    )
    Invoke-Gated -Slot heavy -Name build-release -Command $releaseArgs
    Test-CargoExitOk 'cargo build --release -p daemon -p tracer -p rustling-tulip-native'

    if (-not (Test-Path $installerDir)) {
        New-Item -ItemType Directory -Force -Path $installerDir | Out-Null
    }
    Write-Host "==> Packaging rustling-tulip $version with NSIS..." -ForegroundColor Cyan
    Invoke-Gated -Slot light -Name makensis -Command @(
        $makensis, '/V2', '/WX',
        "/DVERSION=$version", "/DNUMVERSION=$numericVersion",
        "/DBINDIR=$binDir", "/DOUTFILE=$outFile", $nsiScript
    )
    Test-CargoExitOk 'makensis'

    # The release's `.sha256` asset, which the client's updater checks the
    # download against: `<64 lower-case hex>  <file name>`, one LF-terminated line.
    $hashFile = "$outFile.sha256"
    $hash = (Get-FileHash -Algorithm SHA256 -Path $outFile).Hash.ToLowerInvariant()
    $installerName = Split-Path -Leaf $outFile
    [System.IO.File]::WriteAllText($hashFile, "$hash  $installerName`n", [System.Text.UTF8Encoding]::new($false))
    Write-Host $outFile
    Write-Host $hashFile
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

function Assert-Nextest {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Test-Tool 'cargo-nextest' 'Tests run with cargo-nextest; run `.\rt.ps1 setup` to install it.'
}

function Invoke-Test {
    Assert-Nextest
    if ($Validate) {
        # -Validate reads nextest's JSON listings, which a gate call would mix its
        # own lines into: re-run this script under one gate and let the child's
        # calls run directly (it is under that gate, Test-UnderGate). Heavy: its
        # nextest listings keep several cores busy.
        if (-not (Test-UnderGate)) {
            Invoke-Gated -Slot heavy -Name test-validate -Command @(
                'pwsh', '-NoProfile', '-File', $PSCommandPath, 'test', '-Validate'
            )
            exit $LASTEXITCODE
        }
        Test-TestMap
        return
    }
    if ($Reached) { Invoke-ReachedTest; return }
    if ($PrintOnly) { Write-Host 'full suite'; return }
    Invoke-FullSuite
}

function Invoke-FullSuite {
    Write-Host '==> cargo nextest run --cargo-profile test-release (workspace)...' -ForegroundColor Cyan
    $nextestArgs = @(
        'cargo', 'nextest', 'run', '--manifest-path', $ManifestPath,
        '--workspace', '--cargo-profile', 'test-release'
    )
    if ($Rest) { $nextestArgs += $Rest }
    Invoke-Gated -Slot heavy -Name test -Command $nextestArgs
    Test-CargoExitOk 'cargo nextest run'
}

# ---------------------------------------------------------------------------
# Reached tests: changed files -> nextest filters, through tools/test-map.json
# ---------------------------------------------------------------------------

function Get-TestMap {
    return Get-Content -Raw -Path $TestMapPath | ConvertFrom-Json
}

# A glob over repo-relative `/` paths: `**/` is any number of folders, `**`
# anything, `*` and `?` stay inside one path segment.
function ConvertTo-GlobRegex([string]$Glob) {
    $re = [regex]::Escape($Glob)
    $re = $re -replace '\\\*\\\*/', "`u{1}" -replace '\\\*\\\*', "`u{2}"
    $re = $re -replace '\\\*', '[^/]*' -replace '\\\?', '[^/]'
    $re = $re -replace "`u{1}", '(.*/)?' -replace "`u{2}", '.*'
    return "^$re$"
}

function Test-GlobMatch([string]$Path, [object[]]$Globs) {
    foreach ($glob in $Globs) {
        if ($Path -match (ConvertTo-GlobRegex $glob)) { return $true }
    }
    return $false
}

# The commit a reached run diffs against, and how it was chosen. -Base wins;
# else the merge-base of `branch.<name>.base` (or main) with HEAD. When that is
# HEAD itself and the tree is clean (a commit on main), the change is the last
# push's or the last commit's: @{upstream}'s merge-base when it differs, else HEAD~1.
function Get-ReachedBase {
    if ($Base) { return [pscustomobject]@{ Ref = $Base; Label = "-Base $Base" } }
    $branch = (& git -C $ScriptDir rev-parse --abbrev-ref HEAD).Trim()
    $configured = & git -C $ScriptDir config "branch.$branch.base"
    $anchor = if ($LASTEXITCODE -eq 0 -and $configured) { $configured.Trim() } else { 'main' }
    $mergeBase = (& git -C $ScriptDir merge-base $anchor HEAD)
    Test-CargoExitOk "git merge-base $anchor HEAD"
    $mergeBase = $mergeBase.Trim()
    $head = (& git -C $ScriptDir rev-parse HEAD).Trim()
    $clean = -not (& git -C $ScriptDir status --porcelain)
    if ($mergeBase -ne $head -or -not $clean) {
        return [pscustomobject]@{ Ref = $mergeBase; Label = "merge-base of $anchor and HEAD" }
    }
    $upstream = & git -C $ScriptDir merge-base '@{upstream}' HEAD 2>$null
    if ($LASTEXITCODE -eq 0 -and $upstream -and $upstream.Trim() -ne $head) {
        return [pscustomobject]@{ Ref = $upstream.Trim(); Label = "merge-base of @{upstream} and HEAD (HEAD is $anchor's tip, tree clean)" }
    }
    return [pscustomobject]@{ Ref = 'HEAD~1'; Label = "HEAD~1 (HEAD is $anchor's tip, tree clean, no other upstream)" }
}

# Tracked files that differ from the base (committed or not), plus untracked
# ones. A rename counts as both its old and its new path.
function Get-ChangedFile([string]$From) {
    & git -C $ScriptDir rev-parse --verify --quiet "$From^{commit}" *> $null
    if ($LASTEXITCODE -ne 0) { throw "Base '$From' is not a commit in this repository; pass -Base <branch, tag or sha>." }
    $tracked = @(& git -C $ScriptDir -c core.quotePath=false diff --name-only --no-renames $From --)
    Test-CargoExitOk "git diff --name-only $From"
    $untracked = @(& git -C $ScriptDir ls-files --others --exclude-standard)
    Test-CargoExitOk 'git ls-files --others'
    return @($tracked + $untracked | Where-Object { $_ } | Sort-Object -Unique)
}

# Maps changed files to one selection: Kind 'full' (a central or unmapped file),
# 'none' (only ignored files) or 'filter' (the matching rows' filters).
function Resolve-TestSelection([string[]]$Files, [object]$Map) {
    $filters = [System.Collections.Generic.List[string]]::new()
    foreach ($file in $Files) {
        if (Test-GlobMatch $file $Map.central) {
            return [pscustomobject]@{ Kind = 'full'; Reason = "full suite: $file is central" }
        }
        if (Test-GlobMatch $file $Map.ignore) { continue }
        $rows = @($Map.rows | Where-Object { Test-GlobMatch $file $_.paths })
        if ($rows.Count -eq 0) {
            return [pscustomobject]@{ Kind = 'full'; Reason = "full suite: $file is unmapped" }
        }
        foreach ($row in $rows) {
            if (-not $filters.Contains($row.filter)) { $filters.Add($row.filter) }
        }
    }
    if ($filters.Count -eq 0) {
        return [pscustomobject]@{ Kind = 'none'; Reason = 'no tests reached' }
    }
    $joined = ($filters | ForEach-Object { "($_)" }) -join ' | '
    return [pscustomobject]@{ Kind = 'filter'; Reason = "reached: $joined"; Filter = $joined }
}

function Invoke-ReachedTest {
    $from = Get-ReachedBase
    $files = Get-ChangedFile $from.Ref
    $selection = Resolve-TestSelection $files (Get-TestMap)
    Write-Host "==> Reached tests against $($from.Ref): $($from.Label) ($($files.Count) changed file(s))" -ForegroundColor Cyan
    Write-Host $selection.Reason
    if ($PrintOnly -or $selection.Kind -eq 'none') { return }
    if ($selection.Kind -eq 'full') { Invoke-FullSuite; return }
    # A reached set may hold only ignored tests (the live specs): not a failure.
    $nextestArgs = @(
        'cargo', 'nextest', 'run', '--manifest-path', $ManifestPath,
        '--workspace', '--cargo-profile', 'test-release', '--no-tests', 'warn', '-E', $selection.Filter
    )
    if ($Rest) { $nextestArgs += $Rest }
    Invoke-Gated -Slot heavy -Name test-reached -Command $nextestArgs
    Test-CargoExitOk 'cargo nextest run (reached)'
}

# Fails when a native source or test file has no row and is not central, when a
# filter names a binary id nextest does not list, or when a row's filter selects
# no test (nextest rejects it, or it matches nothing, ignored tests included).
function Test-TestMap {
    $map = Get-TestMap
    $problems = [System.Collections.Generic.List[string]]::new()
    $native = Join-Path $ScriptDir 'apps/native'
    $sources = @(Get-ChildItem (Join-Path $native 'src') -Filter '*.rs') +
        @(Get-ChildItem (Join-Path $native 'tests') -Filter '*.rs') +
        @(Get-ChildItem (Join-Path $native 'tests/ui') -Filter '*.rs')
    foreach ($item in $sources) {
        $rel = [IO.Path]::GetRelativePath($ScriptDir, $item.FullName) -replace '\\', '/'
        $covered = (Test-GlobMatch $rel $map.central) -or @($map.rows | Where-Object { Test-GlobMatch $rel $_.paths }).Count -gt 0
        if (-not $covered) { $problems.Add("no row and not central: $rel") }
    }
    Test-MapFilterName $map $problems
    if ($problems.Count -gt 0) {
        $problems | ForEach-Object { Write-Host "    $_" -ForegroundColor Red }
        throw "tools/test-map.json has $($problems.Count) problem(s)."
    }
    Write-Host "test map ok: $(@($map.rows).Count) rows; $($sources.Count) native source and test files covered"
}

# nextest's stderr goes to this log; its tail is printed when a listing fails.
function Invoke-NextestList([string]$LogPath, [string[]]$ListArgs) {
    $output = & cargo nextest list --message-format json @ListArgs 2>> $LogPath
    if ($LASTEXITCODE -ne 0) {
        Get-Content -Path $LogPath -Tail 15 | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkGray }
        return $null
    }
    return $output
}

function Test-MapFilterName([object]$Map, [System.Collections.Generic.List[string]]$Problems) {
    $tmp = Join-Path $ScriptDir '.tmp'
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    $log = Join-Path $tmp 'test-map-validate.log'
    Set-Content -Path $log -Value '' -NoNewline
    $binariesPath = Join-Path $tmp 'test-map-binaries.json'
    $metadataPath = Join-Path $tmp 'test-map-cargo-metadata.json'
    $listArgs = @('--manifest-path', $ManifestPath, '--workspace', '--cargo-profile', 'test-release', '--list-type', 'binaries-only')
    $binaries = Invoke-NextestList $log $listArgs
    if (-not $binaries) { throw "cargo nextest list failed; see $log" }
    Set-Content -Path $binariesPath -Value $binaries
    & cargo metadata --manifest-path $ManifestPath --format-version 1 --all-features 2>> $log | Set-Content -Path $metadataPath
    Test-CargoExitOk 'cargo metadata'
    $binaryIds = @(($binaries | ConvertFrom-Json -AsHashtable)['rust-binaries'].Keys)
    $text = ($Map.rows | ForEach-Object { $_.filter }) -join "`n"
    foreach ($id in ([regex]::Matches($text, 'binary_id\(([^)]+)\)') | ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique)) {
        if ($binaryIds -notcontains $id) { $Problems.Add("unknown binary id: $id") }
    }
    $reuse = @('--binaries-metadata', $binariesPath, '--cargo-metadata', $metadataPath, '--run-ignored', 'all')
    foreach ($row in $Map.rows) {
        $count = Get-RowTestCount $log ($reuse + @('-E', $row.filter))
        if ($count -lt 1) { $Problems.Add("selects no test ($count): $($row.paths -join ', ') -> $($row.filter)") }
    }
}

# The number of tests a filter selects, or -1 when nextest rejects it.
function Get-RowTestCount([string]$LogPath, [string[]]$ListArgs) {
    $json = Invoke-NextestList $LogPath $ListArgs
    if (-not $json) { return -1 }
    $count = 0
    foreach ($suite in ($json | ConvertFrom-Json -AsHashtable)['rust-suites'].Values) {
        $count += @($suite['testcases'].Values | Where-Object { $_['filter-match']['status'] -eq 'matches' }).Count
    }
    return $count
}

function Invoke-Clippy {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> agent names stay behind the seam (tools/check-agent-names.ps1)...' -ForegroundColor Cyan
    & pwsh -NoProfile -File (Join-Path $ScriptDir 'tools/check-agent-names.ps1')
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    Write-Host '==> cargo clippy --all-targets --all-features -- -D warnings...' -ForegroundColor Cyan
    $clippyArgs = @('cargo', 'clippy', '--manifest-path', $ManifestPath, '--all-targets', '--all-features')
    if ($Rest) { $clippyArgs += $Rest }
    $clippyArgs += @('--', '-D', 'warnings')
    Invoke-Gated -Slot heavy -Name clippy -Command $clippyArgs
    Test-CargoExitOk 'cargo clippy'
}

function Invoke-Fmt {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo fmt --all...' -ForegroundColor Cyan
    $fmtArgs = @('cargo', 'fmt', '--manifest-path', $ManifestPath, '--all')
    if ($Rest) { $fmtArgs += $Rest }
    Invoke-Gated -Slot light -Name fmt -Command $fmtArgs
    Test-CargoExitOk 'cargo fmt'
}

function Invoke-Clean {
    Test-Tool 'cargo' 'Install Rust via https://rustup.rs.'
    Write-Host '==> cargo clean...' -ForegroundColor Cyan
    Invoke-Gated -Slot light -Name clean -Command @('cargo', 'clean', '--manifest-path', $ManifestPath)
    Test-CargoExitOk 'cargo clean'
}

# -Features names daemon Cargo features to build with; only native-e2e passes
# any (e2e-loopback-peers), so every other verb builds the shipped daemon.
function Build-DaemonAndTracer([string]$CargoProfile, [string[]]$Features = @()) {
    Assert-Tooling
    $buildArgs = @('build', '--manifest-path', $ManifestPath, '-p', 'daemon', '-p', 'tracer')
    if ($CargoProfile) { $buildArgs += @('--profile', $CargoProfile) }
    elseif ($Release) { $buildArgs += '--release' }
    if ($Features) { $buildArgs += @('--features', (($Features | ForEach-Object { "daemon/$_" }) -join ',')) }
    Write-Host '==> Building daemon + tracer...' -ForegroundColor Cyan
    Invoke-Gated -Slot heavy -Name build-daemon -Command (@('cargo') + $buildArgs)
    Test-CargoExitOk 'cargo build -p daemon -p tracer'
}

function Start-NativeClient {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'Dev script: the calling subcommand is the user gesture.')]
    param()
    # The build goes through the gate; `cargo run` stays up for as long as the
    # user keeps the client open, so it runs directly and holds no slot.
    $buildArgs = @('cargo', 'build', '--manifest-path', $ManifestPath, '-p', 'rustling-tulip-native')
    if ($Release) { $buildArgs += '--release' }
    $modeLabel = if ($Release) { 'release' } else { 'debug' }
    Write-Host "==> Building native client ($modeLabel)..." -ForegroundColor Cyan
    Invoke-Gated -Slot heavy -Name build-native -Command $buildArgs
    Test-CargoExitOk 'cargo build -p rustling-tulip-native'
    $cargoArgs = @('run', '--manifest-path', $ManifestPath, '-p', 'rustling-tulip-native')
    if ($Release) { $cargoArgs += '--release' }
    # Everything after `--` goes to the client, not cargo (the session id).
    if ($Rest) { $cargoArgs += @('--') + $Rest }
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
# tracer are built first in the same profile (test-release, or release with
# -Release). One at a time (the serial-live test group in
# .config/nextest.toml): each spec owns a daemon and, for the smoke specs, a
# client window. The build and the specs run as one gate call, re-entering
# this script under the gate: gated separately, the build and the specs could
# run on different machines, leaving the specs without the daemon binary.
function Invoke-NativeSpecFile([string]$Verb, [string[]]$TestFiles, [string]$Slot, [string]$Name, [string[]]$DaemonFeatures = @()) {
    Assert-Nextest
    if (-not (Test-UnderGate)) {
        $reentry = @('pwsh', '-NoProfile', '-File', $PSCommandPath, $Verb)
        if ($Release) { $reentry += '-Release' }
        Invoke-Gated -Slot $Slot -Name $Name -Command $reentry
        exit $LASTEXITCODE
    }
    $cargoProfile = if ($Release) { 'release' } else { 'test-release' }
    Build-DaemonAndTracer $cargoProfile -Features $DaemonFeatures
    $targets = @($TestFiles | ForEach-Object { '--test'; $_ })
    $names = $TestFiles -join ', '
    Write-Host "==> Running native specs: $names ($cargoProfile)..." -ForegroundColor Cyan
    $specArgs = @(
        'cargo', 'nextest', 'run', '--manifest-path', $ManifestPath,
        '-p', 'rustling-tulip-native', '--cargo-profile', $cargoProfile, '--run-ignored', 'only'
    ) + $targets
    Invoke-Gated -Slot $Slot -Name $Name -Command $specArgs
    Test-CargoExitOk "cargo nextest run -p rustling-tulip-native ($names)"
}

# The e2e daemon admits loopback peers on its tailnet listener (the
# e2e-loopback-peers feature), so e2e_remote can dial it from this machine.
# That build lands only in target\test-release, which the client's daemon
# lookup never searches; -Release would put it in target\release, which it
# does, so it is refused before anything builds.
#
# The sealed-secret spec stores a secret in Credential Manager, which a routed
# run's ssh logon has none of, so it skips there (GATE_BOX_JOB). Outside a gate,
# native-e2e therefore makes two gate calls: the full e2e, which may route, and
# the -SealedSecretOnly re-entry, which .gate/local.toml keeps on this PC. Under
# a gate it runs the full e2e directly, as one call.
function Invoke-NativeE2e {
    if ($Release) {
        throw 'native-e2e builds a test-only daemon (e2e-loopback-peers); it runs in the test-release profile only, so drop -Release.'
    }
    if ($SealedSecretOnly) {
        Invoke-SealedSecretSpec
        return
    }
    if (Test-UnderGate) {
        Invoke-NativeSpecFile -Verb native-e2e -TestFiles @('e2e_live', 'e2e_recover', 'e2e_remote', 'e2e_ssh', 'e2e_multi_host') -Slot heavy -Name e2e -DaemonFeatures @('e2e-loopback-peers')
        return
    }
    Assert-Nextest
    Invoke-Gated -Slot heavy -Name e2e -Command @('pwsh', '-NoProfile', '-File', $PSCommandPath, 'native-e2e')
    $e2eExit = $LASTEXITCODE
    Write-Host "==> Running $SealedSecretSpec on this PC..." -ForegroundColor Cyan
    Invoke-Gated -Slot heavy -Name e2e-sealed -Command @('pwsh', '-NoProfile', '-File', $PSCommandPath, 'native-e2e', '-SealedSecretOnly')
    $sealedExit = $LASTEXITCODE
    if ($e2eExit -ne 0) { exit $e2eExit }
    exit $sealedExit
}

# The spec that needs this PC's Credential Manager.
$SealedSecretSpec = 'live_a_sealed_secret_reaches_the_child_and_no_file'

# Builds the e2e daemon and tracer and runs only $SealedSecretSpec, both in one
# gate call (the re-entry) so the spec finds the daemon it built.
function Invoke-SealedSecretSpec {
    Assert-Nextest
    if (-not (Test-UnderGate)) {
        Invoke-Gated -Slot heavy -Name e2e-sealed -Command @('pwsh', '-NoProfile', '-File', $PSCommandPath, 'native-e2e', '-SealedSecretOnly')
        exit $LASTEXITCODE
    }
    Build-DaemonAndTracer 'test-release' -Features @('e2e-loopback-peers')
    $specArgs = @(
        'cargo', 'nextest', 'run', '--manifest-path', $ManifestPath,
        '-p', 'rustling-tulip-native', '--cargo-profile', 'test-release', '--run-ignored', 'only',
        '--test', 'e2e_recover', '-E', "test(=$SealedSecretSpec)"
    )
    Invoke-Gated -Slot heavy -Name e2e-sealed -Command $specArgs
    Test-CargoExitOk "cargo nextest run -p rustling-tulip-native ($SealedSecretSpec)"
}

function Invoke-NativeSmoke { Invoke-NativeSpecFile -Verb native-smoke -TestFiles 'smoke_window' -Slot light -Name smoke }

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
    $cargoArgs = @(
        'cargo', 'run', '--manifest-path', $ManifestPath,
        '-p', 'rustling-tulip-native', '--example', 'shot'
    )
    if ($Release) { $cargoArgs += '--release' }
    $cargoArgs += @('--', $view, $outPath)
    Write-Host "==> Taking a shot of the native client's '$view' view..." -ForegroundColor Cyan
    Invoke-Gated -Slot heavy -Name shot -Command $cargoArgs
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
             Node.js, Rust, C++ Build Tools, cargo-sweep,
             cargo-nextest). Run once after cloning.
  stop       Kill any running daemon and tracers; remove the stale
             handshake.
  restart    Build daemon + tracer, stop the running daemon (sessions
             survive in their tracers), then run the native client (an
             open client is left to reconnect instead).
  test [-Reached [-Base <ref>]] [-PrintOnly] [-Validate]
             `cargo nextest run --cargo-profile test-release` across the
             workspace. -Reached: only the tests the change since <ref>
             reaches (tools/test-map.json; a central or unmapped file
             runs everything). -PrintOnly: print the selection, run
             nothing. -Validate: check the test map.
  clippy     tools/check-agent-names.ps1, then
             `cargo clippy --all-targets --all-features -- -D warnings`.
  fmt        `cargo fmt --all`.
  clean      `cargo clean`.
  native     `cargo build -p daemon -p tracer`, then `cargo run -p
             rustling-tulip-native`. Extra args are passed on (an
             optional session id to focus, or to place in the active tab).
  native-e2e Build daemon (with e2e-loopback-peers) + tracer, then run
             the native client's end-to-end specs (e2e_live, e2e_recover,
             e2e_remote, e2e_ssh, e2e_multi_host) against an isolated daemon
             (needs `node`).
             Test-release only: refuses -Release. Then runs the sealed-secret
             spec again on this PC in its own gate call (it needs Credential
             Manager and skips in a routed run); -SealedSecretOnly is that
             internal re-entry, kept local by .gate/local.toml.
  native-smoke
             Build daemon + tracer, then run the native client's OS smoke
             specs in a cloaked window that never takes focus; checks it
             connects and that posted keys reach the shell.
  native-shot [<view>] [<out.png>] [-Out <path>]
             Write a PNG of the native client's window showing <view>
             (main, main-compact, source-control, diff, settings, settings-claude, spawn, viewer,
             viewer-md, dashboard, connections, connections-ssh, menu, update, chat, summary,
             themes, themes-edit, multi-host, spawn-hosts, settings-hosts;
             default main)
             over a fake daemon with fixed content, then print its path.
             <out.png> is the PNG to write; -Out says the same thing.
             Default: .tmp\shots\<view>.png. No daemon starts; the window
             stays cloaked and never takes focus.
  installer  Build the release binaries, then package them into an NSIS
             setup exe (target\installer\rustling-tulip-<version>-setup.exe)
             and print its path. Always release. Needs NSIS 3:
             `winget install NSIS.NSIS`.
  help       This message.

`launch` and `native` reuse a running daemon whose protocol matches, even
one from an older build: use `restart` after changing the daemon, tracer
or protocol.

Flags:
  -Release           Use the release profile (build, launch, restart,
                     native, native-smoke, native-shot). native-e2e
                     refuses it: its daemon is a test-only build.
  -Out <path>        The PNG native-shot writes.
  -Reached           test: only the tests the change reaches.
  -Base <ref>        test -Reached: the ref to diff against (default:
                     branch.<name>.base, else main's merge-base).
  -PrintOnly         test: print the selection without running it.
  -Validate          test: check tools/test-map.json.

Examples:
  .\rt.ps1                       # build + run the native client
  .\rt.ps1 build -Release        # release binaries, no launch
  .\rt.ps1 setup                 # install/check build prerequisites
  .\rt.ps1 launch -Release       # build + run the release client
  .\rt.ps1 restart               # rebuild, bounce the daemon, run client
  .\rt.ps1 clippy
  .\rt.ps1 test -Reached         # the tests this branch's change reaches
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
    'installer'    { Invoke-Installer }
    'help'      { Show-Help }
    default     { throw "Unknown command: $effective" }
}
