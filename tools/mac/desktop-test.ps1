<#
.SYNOPSIS
Runs the daemon's ignored Keychain round-trip test inside builder's logged-on
desktop session on the Mac mini.

.DESCRIPTION
A gate job on the Mac runs over SSH, outside the logged-on desktop session, so
the login Keychain answers it with -25308 and `rt-mac.ps1 test --run-ignored all`
cannot pass `keychain_store_round_trips_a_value`. This script runs that one test
inside the session instead.

On this PC (no switch) it runs itself with -OnMac as one heavy gate run pinned to
builder@mac-mini (log .tmp/mac-desktop-test.log) and exits with the gate's code.

On the Mac (-OnMac) it checks that builder is the console user, builds the
daemon's rustling-tulipd unit-test binary (test-release profile), writes
.tmp/mac-desktop/<run id>/run.command (the run id is a new GUID each run), which
runs the one test and writes its exit code to result.code in the same folder,
starts it in the desktop session with `open -g -n` (through Terminal.app), waits
for that result.code, printing `waiting for the desktop run (<n> s of <max> s)`
every 30 s, prints the test's output and exits with the test's code. Earlier run
folders are left in place. A locked screen is not detected.

Exit codes:
  0    the test passed
  101  the test failed (libtest's own code)
  3    builder is not the Mac's console user (no logged-on desktop session)
  4    the test binary did not build
  5    no result within -TimeoutSeconds
  any other code from the gate, such as a refused route, is passed through.

.PARAMETER OnMac
Run the Mac side: the console check, the build, the launch and the wait. The PC
side passes it; it is not meant to be typed.

.PARAMETER TimeoutSeconds
How long the Mac side waits for the test's result file, in seconds (default 300).
The PC side passes it on and sets the gate's ceiling 600 s above it.

.EXAMPLE
pwsh -NoProfile -File tools/mac/desktop-test.ps1
#>
[CmdletBinding()]
param(
    [switch] $OnMac,
    [int] $TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$MacBuilder = 'builder@mac-mini'
$DesktopUser = 'builder'
$TestLeaf = 'keychain_store_round_trips_a_value'
# Each run has a folder of its own, so a result an earlier run's test writes
# late is never read as this run's. Old run folders are left alone: another
# run's test may still be writing to its own.
$RunId = [guid]::NewGuid().ToString('N')
$RunDir = Join-Path $RepoRoot ".tmp/mac-desktop/$RunId"
$ResultFile = Join-Path $RunDir 'result.code'
$OutLog = Join-Path $RunDir 'out.log'

function Invoke-FromPc {
    # A nested gate call would queue behind the slot this run already holds.
    if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
        throw 'tools/mac/desktop-test.ps1: run it from this PC, not inside a gate job'
    }
    $gate = Join-Path $RepoRoot 'tools/gate.ps1'
    $log = Join-Path $RepoRoot '.tmp/mac-desktop-test.log'
    # The gate's ceiling stays above the Mac side's own wait, so exit 5 is reached first.
    $ceiling = $TimeoutSeconds + 600
    & pwsh -NoProfile -File $gate run --builder $MacBuilder --cd $RepoRoot --log $log --timeout $ceiling --slot heavy -- `
        pwsh -NoProfile -File tools/mac/desktop-test.ps1 -OnMac -TimeoutSeconds $TimeoutSeconds
    exit $LASTEXITCODE
}

# Exit 3 unless builder owns the console, since `open` starts the .command in
# the console user's session.
function Assert-ConsoleUser {
    $user = (& stat -f '%Su' /dev/console | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $user -ne $DesktopUser) {
        Write-Output "no logged-on desktop session for $DesktopUser on mac-mini (console user: $user); log $DesktopUser in on the Mac and retry"
        exit 3
    }
}

# Builds the daemon binary's unit tests without running them and returns the test
# binary's path; a failed build prints cargo's output and exits 4.
function Build-TestBinary {
    $lines = @(& cargo test -p daemon --profile test-release --bin rustling-tulipd --no-run --message-format=json 2>&1 | ForEach-Object { "$_" })
    $code = $LASTEXITCODE
    $messages = @($lines | Where-Object { $_.StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
    if ($code -ne 0) {
        $lines | Where-Object { -not $_.StartsWith('{') } | Out-Host
        $messages | Where-Object { $_.reason -eq 'compiler-message' } | ForEach-Object { $_.message.rendered } | Out-Host
        Write-Host "cargo test --no-run failed (exit $code)"
        exit 4
    }
    $artifacts = @($messages | Where-Object {
            $_.reason -eq 'compiler-artifact' -and $_.profile.test -and $_.PSObject.Properties['executable'] -and $_.executable
        })
    if ($artifacts.Count -eq 0) { throw 'cargo built no test binary for rustling-tulipd' }
    return $artifacts[-1].executable
}

# The test's full name, as the binary lists it among its ignored tests.
function Find-TestName([string] $Exe) {
    $listed = @(& $Exe --list --ignored)
    if ($LASTEXITCODE -ne 0) { throw "$Exe --list --ignored failed (exit $LASTEXITCODE)" }
    $names = @($listed | Where-Object { $_ -like "*::${TestLeaf}: test" } | ForEach-Object { $_.Substring(0, $_.Length - ': test'.Length) })
    if ($names.Count -ne 1) { throw "expected one ignored test named $TestLeaf in $Exe, found $($names.Count)" }
    return $names[0]
}

function ConvertTo-ZshQuoted([string] $Text) {
    return "'" + $Text.Replace("'", "'\''") + "'"
}

# Writes run.command (LF, executable) and opens it in the desktop session.
function Start-DesktopRun([string] $Exe, [string] $Name) {
    New-Item -ItemType Directory -Force -Path $RunDir | Out-Null
    $tmp = Join-Path $RunDir 'result.tmp'
    $command = Join-Path $RunDir 'run.command'
    $body = @(
        '#!/bin/zsh',
        "cd $(ConvertTo-ZshQuoted $RepoRoot)",
        "$(ConvertTo-ZshQuoted $Exe) --ignored --exact $(ConvertTo-ZshQuoted $Name) --nocapture > $(ConvertTo-ZshQuoted $OutLog) 2>&1",
        "echo `$? > $(ConvertTo-ZshQuoted $tmp) && mv $(ConvertTo-ZshQuoted $tmp) $(ConvertTo-ZshQuoted $ResultFile)",
        'exit'
    )
    [IO.File]::WriteAllText($command, ($body -join "`n") + "`n")
    & chmod +x $command
    if ($LASTEXITCODE -ne 0) { throw "chmod +x $command failed (exit $LASTEXITCODE)" }
    & open -g -n $command
    if ($LASTEXITCODE -ne 0) { throw "open -g -n $command failed (exit $LASTEXITCODE)" }
}

function Show-OutLog {
    if (Test-Path -LiteralPath $OutLog) { Get-Content -LiteralPath $OutLog | Write-Output }
    else { Write-Output "(no output yet at $OutLog)" }
}

# Waits once a second for this run's result.code; on timeout prints the output
# so far and exits 5. The test runs outside the gate job's process group, so a
# line every 30 s keeps the gate's stall watchdog from ending a quiet wait.
function Wait-ForResult {
    for ($second = 0; $second -lt $TimeoutSeconds; $second++) {
        if (Test-Path -LiteralPath $ResultFile) { return [int]((Get-Content -LiteralPath $ResultFile -Raw).Trim()) }
        if ($second -gt 0 -and $second % 30 -eq 0) { Write-Host "waiting for the desktop run ($second s of $TimeoutSeconds s)" }
        Start-Sleep -Seconds 1
    }
    Show-OutLog | Out-Host
    Write-Host "no result from the desktop run within $TimeoutSeconds s ($ResultFile)"
    exit 5
}

function Invoke-OnMac {
    Assert-ConsoleUser
    $exe = Build-TestBinary
    $name = Find-TestName $exe
    Write-Output "running $name in $DesktopUser's desktop session"
    Start-DesktopRun $exe $name
    $code = Wait-ForResult
    Show-OutLog | Out-Host
    exit $code
}

if ($OnMac) {
    if (-not $IsMacOS) { throw 'tools/mac/desktop-test.ps1 -OnMac runs on the Mac' }
    Invoke-OnMac
}
else {
    Invoke-FromPc
}
