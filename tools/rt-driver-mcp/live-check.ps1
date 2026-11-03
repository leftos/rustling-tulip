#Requires -Version 7
<#
.SYNOPSIS
    The driver MCP's live spec: builds the daemon, tracer and driven client,
    then runs the ignored specs of rt-driver-mcp against them.
.DESCRIPTION
    Run it as one gate job, so the build and the spec share a machine:
    pwsh -NoProfile -File tools/gate.ps1 run --log .tmp/driver-mcp-live.log --timeout 3600 --slot heavy -- pwsh -NoProfile -File tools/rt-driver-mcp/live-check.ps1
    Exits with the first failing step's exit code.
#>
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$manifest = Join-Path $repo 'Cargo.toml'

$steps = @(
    @('build', '--manifest-path', $manifest, '-p', 'daemon', '-p', 'tracer', '--profile', 'test-release'),
    @('build', '--manifest-path', $manifest, '-p', 'rustling-tulip-native', '--features', 'driver', '--profile', 'test-release'),
    @('nextest', 'run', '--manifest-path', $manifest, '-p', 'rt-driver-mcp', '--cargo-profile', 'test-release', '--run-ignored', 'only')
)
foreach ($step in $steps) {
    [Console]::Error.WriteLine("live-check: cargo $($step -join ' ')")
    & cargo @step
    if ($LASTEXITCODE -ne 0) {
        [Console]::Error.WriteLine("live-check: cargo $($step[0]) failed with exit code $LASTEXITCODE")
        exit $LASTEXITCODE
    }
}
