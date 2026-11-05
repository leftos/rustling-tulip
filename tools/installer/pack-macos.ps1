<#
.SYNOPSIS
  Builds the macOS app bundle rustling-tulip.app (arm64, ad-hoc signed), its disk image and the
  updater zip with its .sha256.

.DESCRIPTION
  Runs on a Mac from the repo root or anywhere in the tree. Builds the three release binaries
  (unless -SkipBuild), assembles rustling-tulip.app in a fresh staging folder under -OutDir,
  ad-hoc signs the binaries and then the bundle, and writes beside the staging folder:
    rustling-tulip-<version>-macos-arm64.dmg
    rustling-tulip-<version>-macos-arm64.zip
    rustling-tulip-<version>-macos-arm64.zip.sha256
  <version> is [workspace.package] version in the root Cargo.toml. Developer ID signing and
  notarization are not done here.

.PARAMETER OutDir
  Where the staging folder and the three assets go. Default: target/installer under the repo root.

.PARAMETER SkipBuild
  Use the binaries already in target/release instead of running cargo build --release.
#>
param(
  [string]$OutDir,
  [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $IsMacOS) { throw 'pack-macos.ps1 builds a macOS app bundle and runs only on macOS' }

function Assert-LastExit([string]$What) {
  if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $OutDir) { $OutDir = Join-Path $root 'target/installer' }
$OutDir = [IO.Path]::GetFullPath($OutDir, (Get-Location).Path)

function Get-WorkspaceVersion {
  $inSection = $false
  foreach ($line in Get-Content (Join-Path $root 'Cargo.toml')) {
    if ($line -match '^\s*\[(.+)\]\s*$') { $inSection = $Matches[1] -eq 'workspace.package'; continue }
    if ($inSection -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
  }
  throw 'no [workspace.package] version in Cargo.toml'
}

function Build-AppIcon([string]$WorkDir, [string]$Destination) {
  $source = Join-Path $root 'tools/installer/icon/icon-1024.png'
  $iconset = Join-Path $WorkDir 'icon.iconset'
  New-Item -ItemType Directory -Force -Path $iconset | Out-Null
  foreach ($size in 16, 32, 128, 256, 512) {
    $double = $size * 2
    sips -z $size $size $source --out (Join-Path $iconset "icon_${size}x${size}.png") | Out-Null
    Assert-LastExit "sips $size"
    sips -z $double $double $source --out (Join-Path $iconset "icon_${size}x${size}@2x.png") | Out-Null
    Assert-LastExit "sips $double"
  }
  iconutil -c icns $iconset -o $Destination
  Assert-LastExit 'iconutil'
}

function Build-InfoPlist([string]$Version, [string]$Destination) {
  $numeric = ($Version -split '-')[0]
  $template = Get-Content (Join-Path $root 'tools/installer/macos/Info.plist.template') -Raw
  $plist = $template -replace '__SHORT_VERSION__', $numeric -replace '__BUNDLE_VERSION__', $numeric
  [IO.File]::WriteAllText($Destination, $plist.Replace("`r`n", "`n"))
}

$version = Get-WorkspaceVersion
$binaries = 'rustling-tulip-native', 'rustling-tulipd', 'rt-tracer'
$release = Join-Path $root 'target/release'

if (-not $SkipBuild) {
  cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p rustling-tulip-native -p daemon -p tracer
  Assert-LastExit 'cargo build --release'
}
foreach ($bin in $binaries) {
  if (-not (Test-Path -LiteralPath (Join-Path $release $bin) -PathType Leaf)) { throw "missing $release/$bin (build it, or drop -SkipBuild)" }
  # The assets are named arm64, so every binary must be arm64 alone.
  $archs = (lipo -archs (Join-Path $release $bin)).Trim()
  Assert-LastExit "lipo -archs $bin"
  if ($archs -ne 'arm64') { throw "$release/$bin is built for '$archs', not arm64" }
}

$staging = Join-Path $OutDir 'macos-staging'
if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
$dmgRoot = Join-Path $staging 'dmg-root'
$app = Join-Path $dmgRoot 'rustling-tulip.app'
$macOS = Join-Path $app 'Contents/MacOS'
$resources = Join-Path $app 'Contents/Resources'
New-Item -ItemType Directory -Force -Path $macOS, $resources | Out-Null

foreach ($bin in $binaries) {
  /bin/cp -p (Join-Path $release $bin) (Join-Path $macOS $bin)
  Assert-LastExit "cp $bin"
}
Build-AppIcon $staging (Join-Path $resources 'icon.icns')
Remove-Item -LiteralPath (Join-Path $staging 'icon.iconset') -Recurse -Force
Build-InfoPlist $version (Join-Path $app 'Contents/Info.plist')

# Inside out: each binary, then the bundle, which seals Info.plist and Resources.
foreach ($bin in $binaries) {
  codesign --force --sign - (Join-Path $macOS $bin)
  Assert-LastExit "codesign $bin"
}
codesign --force --sign - $app
Assert-LastExit 'codesign rustling-tulip.app'

$base = "rustling-tulip-$version-macos-arm64"
$zip = Join-Path $OutDir "$base.zip"
$sha = "$zip.sha256"
$dmg = Join-Path $OutDir "$base.dmg"
foreach ($old in $zip, $sha, $dmg) {
  if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Force }
}

ditto -c -k --keepParent $app $zip
Assert-LastExit 'ditto'
$hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText($sha, "$hash  $(Split-Path -Leaf $zip)`n")

ln -s /Applications (Join-Path $dmgRoot 'Applications')
Assert-LastExit 'ln -s /Applications'
hdiutil create -volname rustling-tulip -srcfolder $dmgRoot -ov -format UDZO $dmg
Assert-LastExit 'hdiutil create'

$dmg
$zip
$sha
