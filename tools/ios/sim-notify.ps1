#-------------------------------------------------------------------------------
# tools/ios/sim-notify.ps1 -- build the phone app with its notification service
# extension, run it in the iPhone Simulator on the Mac mini builder and check
# that the system knows the extension and that a push does not crash it.
#
# On this PC (no switch) it runs itself with -OnMac as one heavy gate run pinned
# to the Mac (log .tmp/ios-sim-notify.log), which echoes the files' contents;
# the gate brings .tmp/ios/* back only on a failure. On
# the Mac (-OnMac) it builds the simulator bundle (tools/ios/build.ps1), boots
# the iPhone 17 on iOS 27.0 unless it is already booted, installs the app,
# launches it once with `--push-provisional`, which makes the app ask for
# provisional notification permission (no prompt) and its device token at start,
# and waits for its "phone app started" log line, so the system registers the
# extension, then for its "phone: device token received" or "phone: provisional
# push permission" line, so the app holds the permission before the push. Then it:
#   1. saves pluginkit's list of notification service extensions to
#      .tmp/ios/pluginkit.txt, and fails unless it names the extension;
#   2. sends a mutable-content push with an `e` key that decrypts to nothing,
#      waits 10 s, saves the extension's log lines to .tmp/ios/notify-log.txt
#      and any crash report of the extension from this run to
#      .tmp/ios/notify-crash.txt; it prints whether the simulator accepted the
#      push, whether the log shows the extension launching and whether a crash
#      report exists, and fails when the push is refused or the extension
#      crashed (whether it ran is a measurement only);
#   3. waits up to 60 s each for the app's "phone: provisional push permission
#      granted" line and for "phone: the app delegate received a device token"
#      or "phone: the app delegate failed to register for pushes", which the
#      methods the app adds to its delegate log when UIKit calls them, and fails
#      without them or on a zombie message (the app runs with NSZombieEnabled).
#      An ad hoc simulator bundle has no aps-environment entitlement, so UIKit
#      calls the failure method.
# Then it stops the app and shuts the simulator down again if this run booted it.
#
#   pwsh -NoProfile -File tools/ios/sim-notify.ps1
#
# Prints the three measurements, the app's push log lines and the three file
# paths on success.
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
$NotifyId = 'dev.leftos.rustling-tulip.phone.notify'
$NotifyProcess = 'RustlingTulipNotify'
$ExtensionPoint = 'com.apple.usernotifications.service'
$DeviceName = 'iPhone 17'
$RuntimeMark = 'iOS-27-0'
$ReadyLine = 'phone app started'
# The app's answer to `--push-provisional`: the token, or the permission's answer.
$PushLines = @('phone: device token received', 'phone: provisional push permission')
$ProvisionalArg = '--push-provisional'
$GrantedLine = 'phone: provisional push permission granted'
# Logged by the remote-notification methods the app adds to its delegate, when UIKit calls them.
$HookLines = @('phone: the app delegate received a device token', 'phone: the app delegate failed to register for pushes')
# What NSZombieEnabled writes when a freed object is messaged.
$ZombieMark = 'message sent to deallocated instance'
$OutDir = Join-Path $RepoRoot '.tmp/ios'
$PluginList = Join-Path $OutDir 'pluginkit.txt'
$NotifyLog = Join-Path $OutDir 'notify-log.txt'
$NotifyCrash = Join-Path $OutDir 'notify-crash.txt'
$Payload = Join-Path $OutDir 'notify-push.json'
$RunFiles = @('pluginkit.txt', 'notify-log.txt', 'notify-crash.txt', 'notify-push.json', 'app.log', 'app-err.log')

function Assert-LastExit([string] $Command) {
    if ($LASTEXITCODE -ne 0) { throw "$Command failed (exit $LASTEXITCODE)" }
}

function Invoke-FromPc {
    # A nested gate call would queue behind the slot this run already holds.
    if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
        throw 'tools/ios/sim-notify.ps1: run it from this PC, not inside a gate job'
    }
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    # Files left from an earlier run must not pass for this one.
    foreach ($name in $RunFiles) {
        $path = Join-Path $OutDir $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
    $gate = Join-Path $RepoRoot 'tools/gate.ps1'
    $log = Join-Path $RepoRoot '.tmp/ios-sim-notify.log'
    & pwsh -NoProfile -File $gate run --builder $MacBuilder --cd $RepoRoot --log $log --timeout 3600 --slot heavy -- `
        pwsh -NoProfile -File tools/ios/sim-notify.ps1 -OnMac
    $code = $LASTEXITCODE
    if ($code -ne 0) { exit $code }
    if (-not (Test-Path $PluginList)) {
        Write-Warning "the gate brought back no .tmp/ios files; their contents are in $log"
        return
    }
    Write-Output $PluginList $NotifyLog $NotifyCrash
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

function Clear-RunFiles {
    New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
    foreach ($name in $RunFiles) {
        $path = Join-Path $OutDir $name
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path }
    }
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

function Write-TextFile([string] $Path, [string[]] $Lines) {
    $text = (($Lines | ForEach-Object { "$_" }) -join "`n") + "`n"
    [IO.File]::WriteAllText($Path, $text)
}

# The system's list of notification service extensions must name this one.
function Test-PluginList([string] $Udid) {
    $out = @(& xcrun simctl spawn $Udid pluginkit -m -A -D -v -p $ExtensionPoint 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    Write-TextFile $PluginList $out
    if ($code -ne 0) { throw "pluginkit -m failed (exit $code); output in $PluginList" }
    if (-not ($out -match [regex]::Escape($NotifyId))) { throw "pluginkit does not list $NotifyId; output in $PluginList" }
    Write-Output "pluginkit lists $NotifyId"
}

function Send-Push([string] $Udid) {
    $json = '{"aps":{"alert":{"title":"Rustling Tulip","body":"A session changed. Open the app to see it."},"mutable-content":1},"e":"AAAA"}'
    Write-TextFile $Payload @($json)
    $out = @(& xcrun simctl push $Udid $BundleId $Payload 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    $verdict = if ($code -eq 0) { 'yes' } else { "no (exit $code)" }
    Write-Output "push accepted by the simulator: $verdict"
    if ($code -ne 0) { Write-Output ($out -join "`n") }
    $script:PushExit = $code
}

function Save-NotifyLog([string] $Udid) {
    $predicate = "process == `"$NotifyProcess`" OR eventMessage CONTAINS `"rustling-tulip.phone.notify`""
    $out = @(& xcrun simctl spawn $Udid log show --last 2m --style compact --predicate $predicate 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    Write-TextFile $NotifyLog $out
    if ($code -ne 0) { throw "log show failed (exit $code); output in $NotifyLog" }
    return (@(Get-ExtensionEntry $out).Count -gt 0)
}

# The log entries the extension's own process wrote: `<date> <time> <type> <process>[<pid>:...`.
# The header repeats the predicate and installers quote the bundle's executable name, so a
# bare match on the name is not enough.
function Get-ExtensionEntry([string[]] $Lines) {
    $pattern = '^\d{4}-\d{2}-\d{2} \S+ \S+ ' + [regex]::Escape($NotifyProcess) + '\['
    return @($Lines | Where-Object { $_ -match $pattern })
}

# Crash reports of the extension written since $Since, in the simulator's own
# DiagnosticReports and in the Mac's, where simulator crashes land too.
function Save-CrashList([string] $Udid, [datetime] $Since) {
    $folders = @(
        (Join-Path $HOME "Library/Developer/CoreSimulator/Devices/$Udid/data/Library/Logs/DiagnosticReports"),
        (Join-Path $HOME 'Library/Logs/DiagnosticReports')
    )
    $found = @(foreach ($folder in $folders) {
            if (Test-Path -LiteralPath $folder) {
                Get-ChildItem -LiteralPath $folder -Recurse -File -Filter "$NotifyProcess*" |
                    Where-Object { $_.LastWriteTime -ge $Since } | ForEach-Object { $_.FullName }
            }
        })
    $lines = if ($found.Count -gt 0) { $found } else { @("no crash report for $NotifyProcess since $($Since.ToString('o'))") }
    Write-TextFile $NotifyCrash $lines
    return , $found
}

function Measure-Push([string] $Udid, [datetime] $Since) {
    Send-Push $Udid
    Start-Sleep -Seconds 10
    $launched = Save-NotifyLog $Udid
    $crashes = Save-CrashList $Udid $Since
    Write-Output "extension process in the log: $(if ($launched) { 'yes' } else { 'no' })"
    Write-Output "crash report: $(if ($crashes.Count -gt 0) { $crashes -join ', ' } else { 'none' })"
    if ($crashes.Count -gt 0) { throw "the extension crashed; reports listed in $NotifyCrash" }
    if ($script:PushExit -ne 0) { throw "the simulator refused the push (exit $($script:PushExit))" }
}

# The app must hold provisional permission, and UIKit must have called one of the methods the
# app added to its delegate: with a token, or with the registration's error. A zombie
# message (NSZombieEnabled) means a freed object was messaged.
function Assert-PushHook([string[]] $Logs, [Diagnostics.Process] $Launch) {
    Wait-ForLine $Logs $Launch @($GrantedLine)
    Wait-ForLine $Logs $Launch $HookLines
    $present = @($Logs | Where-Object { Test-Path -LiteralPath $_ })
    if (Select-String -LiteralPath $present -SimpleMatch $ZombieMark -Quiet) {
        throw "the app messaged a freed object ('$ZombieMark' in its log)"
    }
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
    $since = Get-Date
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
        $logs = @((Join-Path $OutDir 'app.log'), (Join-Path $OutDir 'app-err.log'))
        # simctl hands SIMCTL_CHILD_* to the app without the prefix: a freed object messaged fails loudly.
        $env:SIMCTL_CHILD_NSZombieEnabled = 'YES'
        try {
            $launch = Start-Process -FilePath 'xcrun' -PassThru -RedirectStandardOutput $logs[0] -RedirectStandardError $logs[1] `
                -ArgumentList @('simctl', 'launch', '--console-pty', '--terminate-running-process', $udid, $BundleId, $ProvisionalArg)
        }
        finally { $env:SIMCTL_CHILD_NSZombieEnabled = $null }
        Wait-ForLine $logs $launch @($ReadyLine)
        Wait-ForLine $logs $launch $PushLines
        Test-PluginList $udid
        Measure-Push $udid $since
        Assert-PushHook $logs $launch
    }
    finally {
        Stop-Run $udid $launch $booted
        Show-RunFile
        Show-PushLine $logs
    }
}

# The app's own push lines, which come back only in the run's log on success.
function Show-PushLine([string[]] $Logs) {
    Write-Output '----- app push lines'
    foreach ($log in $Logs) {
        if ($log -and (Test-Path -LiteralPath $log)) {
            Select-String -LiteralPath $log -SimpleMatch ($PushLines + $HookLines + @('phone: no device token', $ZombieMark)) |
                ForEach-Object { $_.Line }
        }
    }
}

# The files' contents into the run's log, which comes back whether or not the files do. Of the
# system log only the extension's own entries are shown: other processes' lines carry words
# the gate reads as a failure.
function Show-RunFile {
    foreach ($path in @($PluginList, $NotifyLog, $NotifyCrash)) {
        Write-Output "----- $path"
        if (-not (Test-Path -LiteralPath $path)) { Write-Output '(not written)'; continue }
        $lines = @(Get-Content -LiteralPath $path)
        if ($path -eq $NotifyLog) {
            $lines = @(Get-ExtensionEntry $lines)
            Write-Output "($($lines.Count) entries from the $NotifyProcess process)"
        }
        $lines
    }
}

if ($OnMac) {
    if (-not $IsMacOS) { throw 'tools/ios/sim-notify.ps1 -OnMac runs on the Mac' }
    Invoke-OnMac
}
else {
    Invoke-FromPc
}
