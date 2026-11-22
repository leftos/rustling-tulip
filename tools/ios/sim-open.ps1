#-------------------------------------------------------------------------------
# tools/ios/sim-open.ps1 -- build the phone app, run it in the iPhone Simulator
# on the Mac mini builder and check that it opens a web link through iOS.
#
# On this PC (no switch) it runs itself with -OnMac as one heavy gate run pinned
# to the Mac (log .tmp/ios-sim-open.log). On the Mac (-OnMac) it builds the
# simulator bundle (tools/ios/build.ps1), boots the iPhone 17 on iOS 27.0
# unless it is already booted, installs the app and launches it once with
# `--open-selftest`, which makes the app open https://example.com/ through
# UIApplication once its window shows. It waits up to 60 s for the app's
# "phone app started" line, then up to 60 s for "phone: url opened
# success=true", which the app logs once iOS has opened the page, and fails
# without either. Then it stops the app and shuts the simulator down again if
# this run booted it.
#
#   pwsh -NoProfile -File tools/ios/sim-open.ps1
#
# Prints the app's open lines on success.
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
$SelftestArg = '--open-selftest'
# tracing writes the message, then its fields as name=value.
$OpenedLine = 'phone: url opened success=true'
# The app's other open lines, shown when the page never opens.
$OpenLines = @('phone: url opened', 'phone: open self-test started', 'phone: iOS refused', 'phone: the phone opens only',
    'phone: the phone could not read', 'phone: the open self-test was refused')
$OutDir = Join-Path $RepoRoot '.tmp/ios'
$RunFiles = @('open-app.log', 'open-app-err.log')

function Assert-LastExit([string] $Command) {
    if ($LASTEXITCODE -ne 0) { throw "$Command failed (exit $LASTEXITCODE)" }
}

function Clear-RunFiles {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    # Files left from an earlier run must not pass for this one.
    foreach ($name in $RunFiles) {
        $path = Join-Path $OutDir $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
}

function Invoke-FromPc {
    # A nested gate call would queue behind the slot this run already holds.
    if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
        throw 'tools/ios/sim-open.ps1: run it from this PC, not inside a gate job'
    }
    Clear-RunFiles
    $gate = Join-Path $RepoRoot 'tools/gate.ps1'
    $log = Join-Path $RepoRoot '.tmp/ios-sim-open.log'
    & pwsh -NoProfile -File $gate run --builder $MacBuilder --cd $RepoRoot --log $log --timeout 3600 --slot heavy -- `
        pwsh -NoProfile -File tools/ios/sim-open.ps1 -OnMac
    exit $LASTEXITCODE
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
    & xcrun simctl bootstatus $Udid -b | Out-Host
    Assert-LastExit "xcrun simctl bootstatus $Udid -b"
}

# Waits up to 60 s for a line holding any of $Lines in the app's logs.
function Wait-ForLine([string[]] $Logs, [Diagnostics.Process] $Launch, [string[]] $Lines) {
    for ($second = 0; $second -lt 60; $second++) {
        foreach ($log in $Logs) {
            if ((Test-Path $log) -and (Select-String -LiteralPath $log -SimpleMatch $Lines -Quiet)) { return }
        }
        if ($Launch.HasExited) { break }
        Start-Sleep -Seconds 1
    }
    $tail = foreach ($log in $Logs) { if (Test-Path $log) { Get-Content -LiteralPath $log -Tail 30 } }
    throw "no '$($Lines -join "' or '")' line from the app within 60 s; log tail:`n$($tail -join "`n")"
}

function Stop-Run([string] $Udid, [Diagnostics.Process] $Launch, [bool] $Booted) {
    & xcrun simctl terminate $Udid $BundleId
    if ($LASTEXITCODE -ne 0) { Write-Warning "xcrun simctl terminate failed (exit $LASTEXITCODE)" }
    # The page opened in Safari; close it so the simulator is left as found.
    & xcrun simctl terminate $Udid com.apple.mobilesafari
    if ($LASTEXITCODE -ne 0) { Write-Warning "xcrun simctl terminate Safari failed (exit $LASTEXITCODE)" }
    try {
        if ($Launch -and -not $Launch.HasExited) { Stop-Process -Id $Launch.Id -Force }
    }
    catch { Write-Warning "stopping the launch process failed: $_" }
    if ($Booted) {
        & xcrun simctl shutdown $Udid
        if ($LASTEXITCODE -ne 0) { Write-Warning "xcrun simctl shutdown failed (exit $LASTEXITCODE)" }
    }
}

# The app's own open lines, into the run's log.
function Show-OpenLine([string[]] $Logs) {
    Write-Output '----- app open lines'
    foreach ($log in $Logs) {
        if ($log -and (Test-Path -LiteralPath $log)) {
            Select-String -LiteralPath $log -SimpleMatch $OpenLines | ForEach-Object { $_.Line }
        }
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
    $logs = @()
    try {
        if ($device.state -ne 'Booted') {
            # Set before the boot, so a failed wait still shuts it down.
            $booted = $true
            Start-Simulator $udid
        }
        & xcrun simctl install $udid $app
        Assert-LastExit "xcrun simctl install $udid"
        $logs = @((Join-Path $OutDir 'open-app.log'), (Join-Path $OutDir 'open-app-err.log'))
        $launch = Start-Process -FilePath 'xcrun' -PassThru -RedirectStandardOutput $logs[0] -RedirectStandardError $logs[1] `
            -ArgumentList @('simctl', 'launch', '--console-pty', '--terminate-running-process', $udid, $BundleId, $SelftestArg)
        Wait-ForLine $logs $launch @($ReadyLine)
        Wait-ForLine $logs $launch @($OpenedLine)
        Write-Output "the web link opened"
    }
    finally {
        Stop-Run $udid $launch $booted
        Show-OpenLine $logs
    }
}

if ($OnMac) {
    if (-not $IsMacOS) { throw 'tools/ios/sim-open.ps1 -OnMac runs on the Mac' }
    Invoke-OnMac
}
else {
    Invoke-FromPc
}
