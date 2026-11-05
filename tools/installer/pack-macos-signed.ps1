<#
.SYNOPSIS
  Runs on this PC: builds the Developer ID signed, notarized and stapled macOS app, disk image and
  update zip on the Mac mini builder, through the gate.

.DESCRIPTION
  Reads the owner's macOS signing set (the Developer ID Application .p12, its password, the App
  Store Connect notary key and the ids, shared with yaat) from Private Config on G: into this
  process's environment alone, under yaat's variable names, then starts one pinned heavy gate run
  of `tools/installer/pack-macos.ps1 -Sign` on builder@mac-mini. The variables reach the Mac as
  bare --env names: no value is ever on a command line, in a file in the repo or in a log. The
  log is .tmp/pack-macos.log; the gate brings the three assets back into target/installer.
#>
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$signing = 'G:\My Drive\Private Config\yaat\secrets\macos-signing'
$macBuilder = 'builder@mac-mini'

if (-not (Test-Path -LiteralPath $signing -PathType Container)) { throw "no signing folder at $signing (is Google Drive mounted as G:?)" }
foreach ($file in 'devid-application.p12', 'p12-password.txt', 'ids.env') {
  if (-not (Test-Path -LiteralPath (Join-Path $signing $file) -PathType Leaf)) { throw "missing $file in $signing" }
}
$p8 = @(Get-ChildItem -LiteralPath $signing -Filter 'AuthKey_*.p8' -File)
if ($p8.Count -ne 1) { throw "expected one AuthKey_*.p8 in $signing, found $($p8.Count)" }

# yaat's names (release-macos.yml's secrets), the Application certificate and the notary key only.
$env:MACOS_DEVID_APP_CERT_P12_BASE64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $signing 'devid-application.p12')))
$env:MACOS_DEVID_CERT_PASSWORD = (Get-Content -LiteralPath (Join-Path $signing 'p12-password.txt') -Raw).Trim()
$env:MACOS_NOTARY_API_KEY_P8_BASE64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($p8[0].FullName))
foreach ($line in Get-Content -LiteralPath (Join-Path $signing 'ids.env')) {
  if ($line -match '^\s*(NOTARY_API_KEY_ID|NOTARY_API_ISSUER_ID|SIGN_APP_IDENTITY)\s*=\s*(.+?)\s*$') {
    $key = $Matches[1]
    $value = $Matches[2]
    if ($value -match '^"(.*)"$' -or $value -match "^'(.*)'$") { $value = $Matches[1] }
    [Environment]::SetEnvironmentVariable("MACOS_$key", $value)
  }
}

$names = 'MACOS_DEVID_APP_CERT_P12_BASE64', 'MACOS_DEVID_CERT_PASSWORD', 'MACOS_SIGN_APP_IDENTITY',
  'MACOS_NOTARY_API_KEY_P8_BASE64', 'MACOS_NOTARY_API_KEY_ID', 'MACOS_NOTARY_API_ISSUER_ID'
foreach ($name in $names) {
  if (-not [Environment]::GetEnvironmentVariable($name)) { throw "$name is empty after reading $signing (check ids.env)" }
}

# Names only on the gate's command line; the gate reads each value from this process's environment.
$gateArguments = @('run', '--cd', $root.Replace('\', '/'), '--log', '.tmp/pack-macos.log', '--timeout', '3600', '--slot', 'heavy',
  '--builder', $macBuilder)
foreach ($name in $names) { $gateArguments += '--env', $name }
$gateArguments += '--', 'pwsh', '-NoProfile', '-File', 'tools/installer/pack-macos.ps1', '-Sign'

& pwsh -NoProfile -File (Join-Path $root 'tools/gate.ps1') @gateArguments
exit $LASTEXITCODE
