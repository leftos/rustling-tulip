#-------------------------------------------------------------------------------
# tools/rt-mac.ps1 -- one cargo command on the Mac mini builder, as a heavy gate
# run pinned to it and logged under .tmp/.
#
# The script declares no parameters on purpose. PowerShell binds a script's
# parameters before its body runs, so a named parameter here -- or in rt.ps1,
# which forwards to this script -- would swallow cargo's own arguments: `--test`
# binds to -Test and `-p` binds to -PrintOnly. With no param() block, every
# argument after the sub-verb arrives in $args exactly as typed, `--` included.
#
#   pwsh -NoProfile -File tools/rt-mac.ps1 check|build|clippy|test [<cargo args>]
#
# RT_MAC_PRINT_ONLY=1 prints the gate command line it would run and exits 0.
#-------------------------------------------------------------------------------
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$ScriptDir = $PSScriptRoot
$RepoRoot = Split-Path -Parent $PSScriptRoot
$MacBuilder = 'builder@mac-mini'

# Colored status on the console is the point of these lines in an interactive dev script.
function Write-Status {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
        Justification = 'Interactive dev script; the colored status line is console UX.')]
    param([string] $Text)
    Write-Host $Text -ForegroundColor Cyan
}

# `build` on the Mac sends `-p <crate>` for each `--package` pair, since the
# routed patterns are `cargo build --workspace --all-targets` and
# `cargo build -p * --all-targets*`.
function Get-MacBuildCommand([string[]]$Extra) {
    if (-not $Extra) { return @('cargo', 'build', '--workspace', '--all-targets') }
    $words = @('cargo', 'build')
    $i = 0
    while ($i -lt $Extra.Count) {
        $word = $Extra[$i]
        if ($word -eq '--package') {
            if ($i + 1 -ge $Extra.Count) { throw 'rt.ps1 mac build: --package needs a crate name' }
            $words += @('-p', $Extra[$i + 1])
            $i += 2
        }
        elseif ($word -like '--package=*') {
            $words += @('-p', $word.Substring('--package='.Length))
            $i++
        }
        else {
            $words += $word
            $i++
        }
    }
    if ($words -notcontains '--all-targets') { $words += '--all-targets' }
    return $words
}

$subVerb = if ($args.Count -ge 1) { $args[0] } else { '' }
if ($subVerb -notin @('check', 'build', 'clippy', 'test')) {
    throw 'rt.ps1 mac: expected check, build, clippy or test'
}
$extra = if ($args.Count -gt 1) { @($args[1..($args.Count - 1)]) } else { @() }
$printOnly = ($env:RT_MAC_PRINT_ONLY -eq '1')

# A nested gate call would queue behind the slot this run already holds.
if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
    throw 'rt.ps1 mac: run it from this PC, not inside a gate job'
}

if ($subVerb -eq 'check') {
    $cargoWords = if ($extra) { @('cargo', 'check') + $extra } else { @('cargo', 'check', '--workspace', '--all-targets') }
}
elseif ($subVerb -eq 'build') {
    $cargoWords = Get-MacBuildCommand $extra
}
elseif ($subVerb -eq 'clippy') {
    if (-not $printOnly) {
        Write-Status '==> agent names stay behind the seam (tools/check-agent-names.ps1)...'
        & pwsh -NoProfile -File (Join-Path $ScriptDir 'check-agent-names.ps1')
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    $cargoWords = if ($extra) { @('cargo', 'clippy') + $extra } else { @('cargo', 'clippy', '--workspace') }
    # Cargo's own options go before the first bare `--`; clippy's after it.
    $split = [array]::IndexOf($cargoWords, '--')
    $cargoSide = if ($split -ge 0) { @($cargoWords[0..($split - 1)]) } else { $cargoWords }
    $clippySide = if ($split -ge 0) { @($cargoWords | Select-Object -Skip ($split + 1)) } else { @() }
    # Never --all-features: the `driver` feature turns on windows crate features.
    if ($cargoSide -notcontains '--all-targets') { $cargoSide += '--all-targets' }
    $cargoWords = $cargoSide + @('--') + $clippySide + @('-D', 'warnings')
}
else {
    $cargoWords = @('cargo', 'nextest', 'run', '--cargo-profile', 'test-release')
    if ($extra) { $cargoWords += $extra } else { $cargoWords += '--workspace' }
}

$tmp = Join-Path $RepoRoot '.tmp'
$log = Join-Path $tmp "rt-mac-$subVerb.log"
$gate = Join-Path $ScriptDir 'gate.ps1'
$gateWords = @(
    'pwsh', '-NoProfile', '-File', $gate, 'run', '--builder', $MacBuilder,
    '--cd', $RepoRoot, '--log', $log, '--timeout', '3600', '--slot', 'heavy', '--'
) + $cargoWords

if ($printOnly) {
    Write-Output ($gateWords -join ' ')
    exit 0
}

if (-not (Test-Path $tmp)) { New-Item -ItemType Directory -Force -Path $tmp | Out-Null }
Write-Status "==> $($cargoWords -join ' ') on $MacBuilder..."
& pwsh -NoProfile -File $gate run --builder $MacBuilder --cd $RepoRoot --log $log --timeout 3600 --slot heavy -- @cargoWords
exit $LASTEXITCODE
