#Requires -Version 7.0
<#
.SYNOPSIS
    Rustling-tulip dev helper -- build, launch, package, test, lint, format,
    clean, and manage the daemon process. One entry point replaces the
    older `launch.ps1` and `stop-daemon.ps1`.

.DESCRIPTION
    Subcommands:

      launch     Build the daemon (cargo) then launch `pnpm tauri dev`. This
                 is the default when no subcommand is given -- same flow as
                 the old `launch.ps1`. With -NoBuild: skip the build step
                 and just launch the existing binaries. With -Release:
                 build the full release artifacts and launch the standalone
                 exe detached (combine with -NoBuild to skip the build).
      build      Build only -- no app launch. Debug by default; with -Release
                 produces the standalone-app artifacts (release daemon,
                 frontend bundle, and `rustling-tulip-app.exe`).
      installer  Run `pnpm tauri build` to produce installer bundles
                 (`target\release\bundle\msi\*.msi` and `nsis\*.exe`).
      stop       Kill any running daemon process(es) and clean the stale
                 handshake file. No-op when nothing is running.
      restart    `stop` followed by `launch`. Use this for "fresh dev loop"
                 after the daemon got into a bad state.
      test       Run `cargo test` across the workspace.
      clippy     Run the strict workspace clippy pass
                 (`--all-targets --all-features -- -D warnings`).
      fmt        Run `cargo fmt --all`.
      clean      Run `cargo clean`.
      help       Print the subcommand summary.

.PARAMETER Command
    The subcommand to run (positional). When omitted, defaults to `launch`.

.PARAMETER Release
    Applies to `build`, `launch`, and `restart`. Selects the release
    profile instead of debug.

.PARAMETER NoBuild
    Applies to `launch`/`restart`. Skip the cargo build step and launch
    the existing binaries as-is. Useful when iterating on frontend code
    or running an already-built release artifact.

.PARAMETER ForceStopDaemon
    Applies to `launch`/`restart`. Stops any running daemon before
    building, regardless of whether cargo decides a rebuild is needed.

.PARAMETER Fast
    Applies to `installer`. Three independent speedups: (1) overrides
    `[profile.release]` tuning via CARGO_PROFILE_RELEASE_* env vars
    (lto=false, codegen-units=16, strip=none), (2) skips frontend
    type-checking (vite-only bundle), (3) disables NSIS LZMA compression
    on the installer payload. Output still lands under
    `target\release\bundle\` -- flipping between -Fast and normal
    release forces a cargo rebuild (different fingerprints). Installer
    .exe is larger; not for shippable artifacts.

.EXAMPLE
    .\rt.ps1                       # = .\rt.ps1 launch
    .\rt.ps1 build -Release
    .\rt.ps1 launch -NoBuild
    .\rt.ps1 launch -Release
    .\rt.ps1 installer
    .\rt.ps1 installer -Fast
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
# $Release, $NoBuild, $ForceStopDaemon, $Rest are read by sub-functions via
# the script scope; PSScriptAnalyzer's parameter-usage check doesn't trace
# that.
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSReviewUnusedParameter', '',
    Justification = 'Top-level params consumed by sub-functions via $script: scope.')]
param(
    [Parameter(Position = 0)]
    [ValidateSet('', 'build', 'launch', 'installer', 'stop', 'restart', 'test', 'clippy', 'fmt', 'clean', 'help')]
    [string]$Command = '',

    [switch]$Release,
    [switch]$NoBuild,
    [switch]$ForceStopDaemon,
    [switch]$Fast,

    # Extra arguments forwarded to the underlying tool (e.g. `cargo test --
    # mytest`). Only meaningful for `launch`, `test`, `clippy`, `fmt`.
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Rest
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

$ScriptDir       = Split-Path -Parent $MyInvocation.MyCommand.Path
$AppDir          = Join-Path $ScriptDir 'apps\tauri-app'
$ManifestPath    = Join-Path $ScriptDir 'Cargo.toml'
$ImageName       = 'rustling-tulipd'
$TracerImageName = 'rt-tracer'
$AppImageName    = 'rustling-tulip-app'
$HandshakeFile   = Join-Path $env:APPDATA 'leftos\rustling-tulip\config\daemon.json'
$SidecarStageDir = Join-Path $AppDir 'src-tauri\binaries'

# Cached host triple from `rustc -vV`. Tauri's build script appends this
# suffix to every externalBin entry and refuses to build when the
# resulting path is missing -- so we stage binaries with the same suffix
# whether we're in debug, release, or installer mode.
$script:HostTriple = $null

function Get-DaemonBin {
    # `$Profile` is a PowerShell automatic variable for the user's profile
    # path; use `$BuildProfile` here to avoid the collision.
    param([string]$BuildProfile)
    Join-Path $ScriptDir "target\$BuildProfile\$ImageName.exe"
}

function Get-TracerBin {
    param([string]$BuildProfile)
    Join-Path $ScriptDir "target\$BuildProfile\$TracerImageName.exe"
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
    # Stops the specified process names. Defaults to daemon-only so
    # the routine "rebuilt the daemon, restart it" path preserves
    # Phase C.3 session reattach -- the surviving tracers keep the
    # claude/codex/shell children alive and the freshly-launched
    # daemon picks them back up. Pass an explicit -Names list when
    # the caller knows it needs to replace specific binaries on disk
    # (cargo lock-retry) or wants a full teardown (`rt.ps1 stop`).
    param(
        [string]$Reason,
        [string[]]$Names = @($ImageName)
    )
    $targetNames = $Names
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
    foreach ($p in $processes) {
        Write-Host "    killing PID $($p.Id) ($($p.ProcessName))"
        Stop-Process -Id $p.Id -Force -ErrorAction Continue
    }
    # Give the OS up to 2s for handles to release.
    for ($i = 0; $i -lt 20; $i++) {
        $remaining = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
        if ($remaining.Count -eq 0) { break }
        Start-Sleep -Milliseconds 100
    }
    $remaining = @(Get-Process -Name $targetNames -ErrorAction SilentlyContinue)
    if ($remaining.Count -gt 0) {
        $names = ($remaining | ForEach-Object { "$($_.ProcessName)#$($_.Id)" }) -join ', '
        throw "Failed to stop $($remaining.Count) process(es) within 2s: $names"
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
        # `tracer` and `tracer-protocol` are included so a tracer-only
        # change still triggers a daemon restart -- the running daemon
        # caches the tracer binary path at startup, and ABI drift between
        # an old daemon and a fresh tracer build will surface as cryptic
        # pipe-handshake errors. Easier to just bounce the daemon.
        if ($line -match '^\s*Compiling\s+(daemon|protocol|tracer(-protocol)?)\b') {
            return $true
        }
    }
    return $false
}

# Resolve the host target triple via `rustc -vV`. Cached for the script's
# lifetime since the result only changes when the host toolchain changes.
function Get-HostTriple {
    if ($script:HostTriple) { return $script:HostTriple }
    $hostLine = rustc -vV | Select-String '^host:'
    if (-not $hostLine) { throw 'Could not parse host triple from `rustc -vV`.' }
    $script:HostTriple = ($hostLine.ToString() -replace '^host:\s*', '').Trim()
    return $script:HostTriple
}

# Stage daemon + tracer sidecars into apps/tauri-app/src-tauri/binaries/
# with the target-triple suffix Tauri's externalBin contract demands.
# Idempotent: only copies when the source is newer or the dest is missing.
# Required for both `tauri dev` and `tauri build` -- the latter's build
# script fails fast otherwise, which is what users hit when running
# `rt.ps1` after `cargo clean` or a fresh clone.
function Sync-SidecarBinaries {
    [CmdletBinding()]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '',
        Justification = 'Syncs the full set of sidecars; plural noun is accurate.')]
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseShouldProcessForStateChangingFunctions', '',
        Justification = 'Dev script: the calling subcommand is the user gesture.')]
    param([string]$BuildProfile)

    $triple = Get-HostTriple
    $ext    = if ($IsWindows) { '.exe' } else { '' }
    New-Item -ItemType Directory -Path $SidecarStageDir -Force | Out-Null

    $sidecars = @(
        @{ Name = $ImageName;       Source = Get-DaemonBin -BuildProfile $BuildProfile },
        @{ Name = $TracerImageName; Source = Get-TracerBin -BuildProfile $BuildProfile }
    )

    foreach ($s in $sidecars) {
        if (-not (Test-Path $s.Source)) {
            throw "Sidecar source missing: $($s.Source). Did the cargo build step run?"
        }
        $dest    = Join-Path $SidecarStageDir "$($s.Name)-$triple$ext"
        $sourceT = (Get-Item $s.Source).LastWriteTimeUtc
        $needsCopy = $true
        if (Test-Path $dest) {
            $destT = (Get-Item $dest).LastWriteTimeUtc
            if ($destT -ge $sourceT) { $needsCopy = $false }
        }
        if ($needsCopy) {
            Copy-Item -Path $s.Source -Destination $dest -Force
            Write-Host "    staged $($s.Name) -> $dest" -ForegroundColor DarkGray
        }
    }
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

# Debug daemon + tracer build. Both binaries are spawned from a
# content-addressed cache (see crates/daemon/src/binary_cache.rs) so the
# shipped target/debug/{rustling-tulipd,rt-tracer}.exe templates are never
# held by a running process -- cargo can overwrite them while the daemon
# and tracers continue running their cached copies. No lock-retry needed.
function Invoke-DebugBuild {
    Write-Host '==> Building daemon + tracer (debug)...' -ForegroundColor Cyan
    $cargoArgs = @('build', '-p', 'daemon', '-p', 'tracer')

    $exit = Invoke-CargoCapture -CargoArgs $cargoArgs
    $output = $script:CargoLines
    # The lock-retry path is gone (binary cache eliminates the file lock),
    # so a running daemon is never killed by the build. The caller's
    # "restart after rebuild" path stays in charge of bouncing the daemon.
    $script:DaemonAlreadyStopped = $false

    Test-CargoExitOk 'cargo build'

    $daemonBin = Get-DaemonBin -BuildProfile 'debug'
    $tracerBin = Get-TracerBin -BuildProfile 'debug'
    if (-not (Test-Path $daemonBin)) { throw "Daemon binary missing after build: $daemonBin" }
    if (-not (Test-Path $tracerBin)) { throw "Tracer binary missing after build: $tracerBin" }

    # Stage sidecars for Tauri's externalBin build-time check. Idempotent
    # -- no-op when the staged copies are already up to date.
    Sync-SidecarBinaries -BuildProfile 'debug'

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
    # The Tauri app exe is loaded directly (no binary cache for it), so
    # rebuilding target/release/rustling-tulip.exe still requires the GUI
    # to be closed first. The daemon and tracer are spawned via the
    # binary cache and don't lock their templates -- they keep running.
    Write-Host '==> Stopping running rustling-tulip GUI (its exe must be free for release rebuild)...' -ForegroundColor Cyan
    $killed = 0
    Get-Process -Name $AppImageName -ErrorAction SilentlyContinue | ForEach-Object {
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

    Write-Host '==> Building daemon + tracer + tauri app (release)...' -ForegroundColor Cyan
    & cargo build --release --manifest-path $ManifestPath -p daemon -p tracer -p rustling-tulip-app --features rustling-tulip-app/custom-protocol
    Test-CargoExitOk 'cargo build --release'

    $appExe    = Get-AppExe
    $daemonExe = Get-DaemonBin -BuildProfile 'release'
    $tracerExe = Get-TracerBin -BuildProfile 'release'
    if (-not (Test-Path $appExe))    { throw "Release app exe missing: $appExe" }
    if (-not (Test-Path $daemonExe)) { throw "Release daemon exe missing: $daemonExe" }
    if (-not (Test-Path $tracerExe)) { throw "Release tracer exe missing: $tracerExe" }

    # Stage sidecars even in the build path -- the release flow runs the
    # standalone .exe directly (no Tauri build script involvement), but
    # keeping the binaries/ folder consistent means subsequent `installer`
    # runs are pure no-ops on the staging step.
    Sync-SidecarBinaries -BuildProfile 'release'
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

    # Launch-only mode: skip the build step entirely.
    if ($NoBuild) {
        if ($Release) { Invoke-ReleaseLaunch } else { Invoke-DevLaunch }
        return
    }

    # Build + launch mode.
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

    # -Fast bundles three independent speedups via a tauri.conf.json
    # merge-patch (written to a file because PowerShell's `Windows`
    # native-command arg passing strips inner double-quotes from `-c
    # <json>`):
    #   1. CARGO_PROFILE_RELEASE_* env vars below cut LTO/codegen-units/
    #      strip on both the explicit cargo build and the one Tauri
    #      kicks off internally. Tauri's CLI has no `--profile`, so env
    #      vars are the only handle.
    #   2. `build.beforeBuildCommand = "pnpm build:fast"` skips `tsc -b`
    #      -- vite alone produces the bundle.
    #   3. `bundle.windows.nsis.compression = "none"` disables LZMA on
    #      the installer payload. The installer .exe gets larger, but
    #      avoiding the LZMA pass saves the most wall time of the three.
    # NOTE: -Fast and normal release share `target/release/`, so
    # flipping between modes invalidates the cargo fingerprint and
    # forces a rebuild. Expected; not for shippable artifacts either way.
    $tauriOverrideFile = $null
    $tauriExtraArgs = @()
    if ($Fast) {
        $tmpDir = Join-Path $ScriptDir '.tmp'
        New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null
        $tauriOverrideFile = Join-Path $tmpDir 'tauri-fast-override.json'
        $overrideJson = '{"build":{"beforeBuildCommand":"pnpm build:fast"},"bundle":{"windows":{"nsis":{"compression":"none"}}}}'
        $overrideJson | Set-Content -Path $tauriOverrideFile -Encoding utf8 -NoNewline
        $tauriExtraArgs = @('-c', $tauriOverrideFile)
    }
    $modeLabel = if ($Fast) { 'release (fast)' } else { 'release' }

    $cargoFastOverrides = @{
        CARGO_PROFILE_RELEASE_LTO            = 'false'
        CARGO_PROFILE_RELEASE_CODEGEN_UNITS  = '16'
        CARGO_PROFILE_RELEASE_STRIP          = 'none'
    }
    $cargoFastSaved = @{}
    if ($Fast) {
        foreach ($k in $cargoFastOverrides.Keys) {
            $cargoFastSaved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
            [Environment]::SetEnvironmentVariable($k, $cargoFastOverrides[$k], 'Process')
        }
    }

    try {
        # Step 1: build the release daemon + tracer. Tauri bundles them
        # as `externalBin` siblings next to the main app exe so
        # daemon_supervisor's `current_exe().parent()` discovery works in
        # the installed layout.
        Write-Host "==> Building $modeLabel daemon + tracer..." -ForegroundColor Cyan
        & cargo build --release --manifest-path $ManifestPath -p daemon -p tracer
        Test-CargoExitOk "cargo build $modeLabel (daemon + tracer)"

        # Step 2: stage the sidecar binaries with the target-triple
        # suffix Tauri expects. Shared helper -- same logic the
        # debug/release builds use, so the binaries/ folder stays
        # consistent across modes.
        Sync-SidecarBinaries -BuildProfile 'release'

        # Step 3: run `pnpm tauri build`. Tauri builds the app exe +
        # frontend bundle and produces the installer per
        # `tauri.conf.json` (NSIS).
        Write-Host "==> Building installer bundles (pnpm tauri build, $modeLabel)..." -ForegroundColor Cyan
        Push-Location $AppDir
        try {
            & pnpm tauri build @tauriExtraArgs @Rest
            Test-CargoExitOk 'pnpm tauri build'
        } finally {
            Pop-Location
        }
        $bundleDir = Join-Path $ScriptDir 'target\release\bundle'
        if (Test-Path $bundleDir) {
            Write-Host "==> Bundles written under $bundleDir" -ForegroundColor Green
        }
    } finally {
        if ($Fast) {
            foreach ($k in $cargoFastOverrides.Keys) {
                $saved = $cargoFastSaved[$k]
                if ($null -eq $saved) {
                    # Original was unset, so the correct restoration is
                    # "remove the variable" -- NOT
                    # SetEnvironmentVariable($k, $null, 'Process'). In
                    # practice that .NET call has been observed to
                    # leave an empty-string value behind in the
                    # PowerShell session, which then poisons every
                    # subsequent cargo invocation (cargo parses
                    # CARGO_PROFILE_RELEASE_CODEGEN_UNITS unconditionally
                    # and rejects an empty integer, breaking even debug
                    # builds). Remove-Item is explicit and survives
                    # PowerShell-version drift.
                    Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue
                } else {
                    [Environment]::SetEnvironmentVariable($k, $saved, 'Process')
                }
            }
            if ($tauriOverrideFile -and (Test-Path $tauriOverrideFile)) {
                Remove-Item -Path $tauriOverrideFile -Force -ErrorAction SilentlyContinue
            }
        }
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

function Invoke-Restart {
    [void](Stop-DaemonProcesses -Reason 'restart requested')
    $script:DaemonAlreadyStopped = $true
    Invoke-Launch
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
  .\rt.ps1 [<command>] [-Release] [-NoBuild] [-ForceStopDaemon] [-Fast] [-- <extra>]

Commands:
  launch     Build the daemon then `pnpm tauri dev` (default if omitted).
             With -NoBuild: skip the build, just launch existing binaries.
             With -Release: build + launch the standalone exe detached.
  build      Build only -- no app launch. Debug by default; -Release for
             the standalone-app release artifacts.
  installer  `pnpm tauri build` -- produces installer bundles under
             target\release\bundle\.
  stop       Kill any running daemon and remove the stale handshake.
  restart    stop + launch. Fresh dev loop after the daemon misbehaves.
  test       `cargo test` across the workspace.
  clippy     `cargo clippy --all-targets --all-features -- -D warnings`.
  fmt        `cargo fmt --all`.
  clean      `cargo clean`.
  help       This message.

Flags:
  -Release           Use the release profile (build/launch/restart).
  -NoBuild           Skip the cargo build step (launch/restart).
  -ForceStopDaemon   Stop the daemon up-front (launch/restart) even when
                     cargo says no rebuild is needed.
  -Fast              Installer-only: (1) override `[profile.release]`
                     via CARGO_PROFILE_RELEASE_* env vars (lto=false,
                     codegen-units=16, strip=none), (2) skip `tsc -b`
                     type-check, (3) disable NSIS LZMA compression.
                     Output still lands in target\release\bundle\.
                     Installer .exe is larger; not for shippable
                     artifacts. Switching between -Fast and normal
                     release forces a cargo rebuild.

Examples:
  .\rt.ps1                       # build + launch dev
  .\rt.ps1 build -Release        # release artifacts, no launch
  .\rt.ps1 launch -NoBuild       # launch existing debug binaries
  .\rt.ps1 launch -Release       # build + launch release exe
  .\rt.ps1 installer             # build installer bundle (shippable)
  .\rt.ps1 installer -Fast       # local installer iteration (faster)
  .\rt.ps1 restart               # kill daemon + relaunch dev
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
