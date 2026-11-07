# Runs one of the phone app's checks for the prek hooks, with the install step
# named when the workspace has no dependencies yet.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet('lint', 'fmt')]
    [string]$Target
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$pwa = Join-Path $root 'apps\pwa'

if (-not (Test-Path -LiteralPath (Join-Path $pwa 'node_modules'))) {
    [Console]::Error.WriteLine('apps/pwa has no node_modules: run pnpm --dir apps/pwa install')
    exit 1
}

$pnpmArgs = if ($Target -eq 'lint') { @('exec', 'oxlint') } else { @('exec', 'oxfmt', '--check') }
& pnpm --dir $pwa @pnpmArgs
exit $LASTEXITCODE
