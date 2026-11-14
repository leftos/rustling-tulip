#-------------------------------------------------------------------------------
# tools/ios/sim-run.ps1 -- build the phone app, run it in the iPhone Simulator on
# the Mac mini builder and bring back a screenshot of its first screen.
#
# On this PC (no switch) it runs itself with -OnMac as one heavy gate run pinned
# to the Mac (log .tmp/ios-sim-run.log); the gate brings .tmp/ios/*.png back on
# a pass and .tmp/ios/* on a failure. On the Mac (-OnMac) it builds the
# simulator bundle (tools/ios/build.ps1), boots the iPhone 17 on iOS 27.0 unless
# it is already booted, installs and launches the app, waits for its
# "phone app started" log line, takes .tmp/ios/sim-first.png, then stops the app
# and shuts the simulator down again if this run booted it.
#
#   pwsh -NoProfile -File tools/ios/sim-run.ps1
#
# Prints the screenshot path on success.
#-------------------------------------------------------------------------------
[CmdletBinding()]
param(
    [switch] $OnMac
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$MacBuilder = 'builder@mac-mini'
$BundleId = 'dev.leftos.rustling-tulip.phone'
$DeviceName = 'iPhone 17'
$RuntimeMark = 'iOS-27-0'
$ReadyLine = 'phone app started'
$OutDir = Join-Path $RepoRoot '.tmp/ios'
$Shot = Join-Path $OutDir 'sim-first.png'

function Assert-LastExit([string] $Command) {
    if ($LASTEXITCODE -ne 0) { throw "$Command failed (exit $LASTEXITCODE)" }
}

function Invoke-FromPc {
    # A nested gate call would queue behind the slot this run already holds.
    if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
        throw 'tools/ios/sim-run.ps1: run it from this PC, not inside a gate job'
    }
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    # A screenshot left from an earlier run must not pass for this one.
    if (Test-Path $Shot) { Remove-Item -LiteralPath $Shot }
    $gate = Join-Path $RepoRoot 'tools/gate.ps1'
    $log = Join-Path $RepoRoot '.tmp/ios-sim-run.log'
    & pwsh -NoProfile -File $gate run --builder $MacBuilder --cd $RepoRoot --log $log --timeout 3600 --slot heavy -- `
        pwsh -NoProfile -File tools/ios/sim-run.ps1 -OnMac
    $code = $LASTEXITCODE
    if ($code -ne 0) { exit $code }
    if (-not (Test-Path $Shot)) { throw "the gate passed but brought back no screenshot at $Shot" }
    Write-Output $Shot
}

function Find-Device {
    $json = & xcrun simctl list devices available -j
    Assert-LastExit 'xcrun simctl list devices available -j'
    $runtimes = ($json -join "`n" | ConvertFrom-Json).devices
    $seen = @()
    foreach ($runtime in $runtimes.PSObject.Properties) {
        foreach ($device in $runtime.Value) {
            $seen += "$($device.name) ($($runtime.Name))"
            if ($device.name -eq $DeviceName -and $runtime.Name -like "*$RuntimeMark") { return $device }
        }
    }
    throw "no available '$DeviceName' on an iOS 27.0 runtime; found: $($seen -join ', ')"
}

function Start-Simulator([string] $Udid) {
    # Its output goes to the console, never into this function's result.
    & xcrun simctl boot $Udid | Out-Host
    Assert-LastExit "xcrun simctl boot $Udid"
}

function Wait-ForBoot([string] $Udid) {
    & xcrun simctl bootstatus $Udid -b | Out-Host
    Assert-LastExit "xcrun simctl bootstatus $Udid -b"
}

# Leaves nothing from an earlier run in the work folder for this run's
# outputs to bring back.
function Clear-RunFiles {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    foreach ($name in @('sim-first.png', 'app.log', 'app-err.log')) {
        $path = Join-Path $OutDir $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
}

function Wait-ForReadyLine([string[]] $Logs, [Diagnostics.Process] $Launch) {
    for ($second = 0; $second -lt 60; $second++) {
        foreach ($log in $Logs) {
            if ((Test-Path $log) -and (Select-String -LiteralPath $log -SimpleMatch $ReadyLine -Quiet)) { return }
        }
        if ($Launch.HasExited) { break }
        Start-Sleep -Seconds 1
    }
    $tail = foreach ($log in $Logs) { if (Test-Path $log) { Get-Content -LiteralPath $log -Tail 30 } }
    throw "no '$ReadyLine' line from the app within 60 s; log tail:`n$($tail -join "`n")"
}

function Stop-Run([string] $Udid, [Diagnostics.Process] $Launch, [bool] $Booted) {
    & xcrun simctl terminate $Udid $BundleId
    if ($LASTEXITCODE -ne 0) { Write-Warning "xcrun simctl terminate failed (exit $LASTEXITCODE)" }
    try {
        if ($Launch -and -not $Launch.HasExited) { Stop-Process -Id $Launch.Id -Force }
    }
    catch { Write-Warning "stopping the launch process failed: $_" }
    if ($Booted) {
        & xcrun simctl shutdown $Udid
        if ($LASTEXITCODE -ne 0) { Write-Warning "xcrun simctl shutdown failed (exit $LASTEXITCODE)" }
    }
}

function Invoke-OnMac {
    Clear-RunFiles
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'build.ps1') -Target sim
    Assert-LastExit 'tools/ios/build.ps1 -Target sim'
    $app = Join-Path $RepoRoot 'target/ios/aarch64-apple-ios-sim/RustlingTulip.app'
    $device = Find-Device
    $udid = $device.udid
    $booted = $false
    $launch = $null
    try {
        if ($device.state -ne 'Booted') {
            Start-Simulator $udid
            # Set before the wait, so a failed wait still shuts it down.
            $booted = $true
            Wait-ForBoot $udid
        }
        & xcrun simctl install $udid $app
        Assert-LastExit "xcrun simctl install $udid"
        $logs = @((Join-Path $OutDir 'app.log'), (Join-Path $OutDir 'app-err.log'))
        $launch = Start-Process -FilePath 'xcrun' -PassThru -RedirectStandardOutput $logs[0] -RedirectStandardError $logs[1] `
            -ArgumentList @('simctl', 'launch', '--console-pty', '--terminate-running-process', $udid, $BundleId)
        Wait-ForReadyLine $logs $launch
        & xcrun simctl io $udid screenshot $Shot
        Assert-LastExit "xcrun simctl io $udid screenshot"
    }
    finally {
        Stop-Run $udid $launch $booted
    }
    Write-Output $Shot
}

if ($OnMac) {
    if (-not $IsMacOS) { throw 'tools/ios/sim-run.ps1 -OnMac runs on the Mac' }
    Invoke-OnMac
}
else {
    Invoke-FromPc
}
