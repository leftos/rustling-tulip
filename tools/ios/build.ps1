#-------------------------------------------------------------------------------
# tools/ios/build.ps1 -- build the phone app (apps/ios) for an iOS target and
# assemble it into a flat app bundle under target/ios/<target>/.
#
# Runs on the Mac (pwsh). The bundle holds the binary and apps/ios/Info.plist,
# its version keys set from the workspace version. A simulator bundle is signed
# ad hoc; a device bundle is left unsigned.
#
#   pwsh -NoProfile -File tools/ios/build.ps1 [-Target sim|device] [-Release]
#
# Prints the bundle path last.
#-------------------------------------------------------------------------------
[CmdletBinding()]
param(
    [ValidateSet('sim', 'device')]
    [string] $Target = 'sim',
    [switch] $Release
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$BinaryName = 'rustling-tulip-ios'
$PlistBuddy = '/usr/libexec/PlistBuddy'

function Assert-LastExit([string] $Command) {
    if ($LASTEXITCODE -ne 0) { throw "$Command failed (exit $LASTEXITCODE)" }
}

function Get-WorkspaceVersion {
    $inSection = $false
    foreach ($line in Get-Content (Join-Path $RepoRoot 'Cargo.toml')) {
        if ($line -match '^\s*\[(.+)\]\s*$') { $inSection = $Matches[1] -eq 'workspace.package'; continue }
        if ($inSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    }
    throw 'no [workspace.package] version in Cargo.toml'
}

function Invoke-CargoBuild([string] $Triple) {
    & rustup target add $Triple
    Assert-LastExit "rustup target add $Triple"
    $cargoArgs = @('build', '-p', $BinaryName, '--target', $Triple)
    if ($Release) { $cargoArgs += '--release' }
    $env:IPHONEOS_DEPLOYMENT_TARGET = '15.0'
    & cargo @cargoArgs
    Assert-LastExit "cargo $($cargoArgs -join ' ')"
}

function Set-PlistValue([string] $Plist, [string] $Key, [string] $Value) {
    & $PlistBuddy -c "Set :$Key $Value" $Plist
    Assert-LastExit "PlistBuddy Set :$Key"
}

function New-AppBundle([string] $Triple) {
    $profileDir = if ($Release) { 'release' } else { 'debug' }
    $binary = Join-Path $RepoRoot "target/$Triple/$profileDir/$BinaryName"
    if (-not (Test-Path $binary)) { throw "no binary at $binary after the build" }
    $app = Join-Path $RepoRoot "target/ios/$Triple/RustlingTulip.app"
    if (Test-Path $app) { Remove-Item -LiteralPath $app -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $app | Out-Null
    Copy-Item -LiteralPath $binary -Destination (Join-Path $app $BinaryName)
    $plist = Join-Path $app 'Info.plist'
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'apps/ios/Info.plist') -Destination $plist
    $numeric = ((Get-WorkspaceVersion) -split '-')[0]
    Set-PlistValue $plist 'CFBundleShortVersionString' $numeric
    Set-PlistValue $plist 'CFBundleVersion' $numeric
    return $app
}

if (-not $IsMacOS) { throw 'tools/ios/build.ps1 runs on the Mac: iOS builds need Xcode' }

$triple = if ($Target -eq 'sim') { 'aarch64-apple-ios-sim' } else { 'aarch64-apple-ios' }
Invoke-CargoBuild $triple
$app = New-AppBundle $triple
if ($Target -eq 'sim') {
    & codesign --force --sign - $app
    Assert-LastExit 'codesign'
}
else {
    Write-Output "unsigned device bundle at $app; signing is a later step"
}
Write-Output $app
