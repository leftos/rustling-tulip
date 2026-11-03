#Requires -Version 7
<#
.SYNOPSIS
    Starts the installed rt-driver-mcp for this checkout, stdio passed
    through; the server `.mcp.json` registers as `rt-driver`.
.DESCRIPTION
    Reads current.txt under %LOCALAPPDATA%\leftos\rustling-tulip\data\driver-mcp\
    and runs that version's exe with --repo-root set to this checkout. It
    never builds: with nothing installed it fails naming install.ps1.
#>
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$root = Join-Path $env:LOCALAPPDATA 'leftos\rustling-tulip\data\driver-mcp'
$current = Join-Path $root 'current.txt'
$install = 'pwsh -NoProfile -File tools/rt-driver-mcp/install.ps1'

if (-not (Test-Path -LiteralPath $current)) {
    [Console]::Error.WriteLine("rt-driver-mcp is not installed (no $current); run $install")
    exit 1
}
$exe = Join-Path $root ((Get-Content -Raw -LiteralPath $current).Trim()) 'rt-driver-mcp.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    [Console]::Error.WriteLine("current.txt names a missing $exe; run $install")
    exit 1
}

# Started without redirection, the server inherits this process's own
# stdin, stdout and stderr, so frames pass through unbuffered.
$start = [System.Diagnostics.ProcessStartInfo]::new($exe)
$start.UseShellExecute = $false
$start.ArgumentList.Add('--repo-root')
$start.ArgumentList.Add($repo)
$server = [System.Diagnostics.Process]::Start($start)
$server.WaitForExit()
exit $server.ExitCode
