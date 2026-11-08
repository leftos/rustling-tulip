#Requires -Version 7
<#
.SYNOPSIS
    Stores the driver token: the token `claude setup-token` prints, which a
    driven run's real Claude logs in with.
.DESCRIPTION
    Run it by hand, once, after `claude setup-token`. It asks for the token
    without echoing it and pipes it to the installed `rt-driver-mcp.exe
    set-token` (the version tools/rt-driver-mcp/install.ps1 names in
    current.txt), which writes the Windows Credential Manager entry
    rustling-tulip-driver / driver-token. The token is never printed or
    logged.
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Fail([string]$message) {
    [Console]::Error.WriteLine("set-driver-token.ps1: $message")
    exit 1
}

$root = Join-Path $env:LOCALAPPDATA 'leftos\rustling-tulip\data\driver-mcp'
$current = Join-Path $root 'current.txt'
if (-not (Test-Path -LiteralPath $current)) {
    Fail "no installed rt-driver-mcp ($current is missing); run tools/rt-driver-mcp/install.ps1 first"
}
$name = ([System.IO.File]::ReadAllText($current)).Trim()
$exe = Join-Path (Join-Path $root $name) 'rt-driver-mcp.exe'
if (-not (Test-Path -LiteralPath $exe)) {
    Fail "current.txt names $name, but $exe is missing; run tools/rt-driver-mcp/install.ps1 again"
}

$secure = Read-Host -AsSecureString 'Paste the token from claude setup-token'
$plain = ConvertFrom-SecureString -SecureString $secure -AsPlainText
try {
    $plain | & $exe set-token
    $code = $LASTEXITCODE
} finally {
    $plain = $null
    $secure.Dispose()
}
if ($code -ne 0) { Fail "rt-driver-mcp set-token failed (exit $code)" }
