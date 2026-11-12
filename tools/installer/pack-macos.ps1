<#
.SYNOPSIS
  Builds the macOS app bundle rustling-tulip.app (arm64), its disk image and the updater zip with
  its .sha256: ad-hoc signed, or with -Sign signed with the Developer ID, notarized and stapled.

.DESCRIPTION
  Runs on a Mac from the repo root or anywhere in the tree. Builds the three release binaries and
  the phone app (unless -SkipBuild), assembles rustling-tulip.app in a fresh staging folder under
  -OutDir with the phone app in Contents/Resources/pwa, signs the binaries and then the bundle,
  and writes beside the staging folder:
    rustling-tulip-<version>-macos-arm64.dmg
    rustling-tulip-<version>-macos-arm64.zip
    rustling-tulip-<version>-macos-arm64.zip.sha256
  <version> is [workspace.package] version in the root Cargo.toml.

  Without -Sign every binary and the bundle are ad-hoc signed. With -Sign they are signed with
  the Developer ID Application identity (hardened runtime, timestamp), the app is notarized and
  stapled, the disk image is signed, notarized and stapled, and Gatekeeper's verdict on both is
  checked before the paths are printed. The signing material arrives in environment variables
  named as yaat's release-macos.yml secrets (tools/installer/README.md "macOS code signing"); it
  is decoded into a temporary keychain added beside the user's keychain list, which is put back,
  and the keychain and decoded files deleted, whatever happens.

.PARAMETER OutDir
  Where the staging folder and the three assets go. Default: target/installer under the repo root.

.PARAMETER SkipBuild
  Use the binaries already in target/release and the phone app already built in apps/pwa/dist,
  instead of running cargo build --release and the phone app's pnpm install and build.

.PARAMETER Sign
  Sign with the Developer ID Application identity, notarize and staple the app and the disk image.
#>
param(
  [string]$OutDir,
  [switch]$SkipBuild,
  [switch]$Sign
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $IsMacOS) { throw 'pack-macos.ps1 builds a macOS app bundle and runs only on macOS' }

function Assert-LastExit([string]$What) {
  if ($LASTEXITCODE -ne 0) { throw "$What failed (exit $LASTEXITCODE)" }
}

# yaat's names for the Application certificate and the notary key (its release-macos.yml secrets).
$signingNames = 'MACOS_DEVID_APP_CERT_P12_BASE64', 'MACOS_DEVID_CERT_PASSWORD', 'MACOS_SIGN_APP_IDENTITY',
  'MACOS_NOTARY_API_KEY_P8_BASE64', 'MACOS_NOTARY_API_KEY_ID', 'MACOS_NOTARY_API_ISSUER_ID'
if ($Sign) {
  foreach ($name in $signingNames) {
    if (-not [Environment]::GetEnvironmentVariable($name)) { throw "-Sign needs the environment variable $name, which is not set" }
  }
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

# Always an array, even of one keychain: a lone string splatted into `security list-keychains -s`
# would pass each character as a keychain path and wreck the user's search list (yaat's pack-macos.ps1).
function Get-UserKeychainList {
  $list = @(security list-keychains -d user | ForEach-Object { $_.Trim().Trim('"') } | Where-Object { $_ })
  $missing = @($list | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) })
  if (($list.Count -eq 0) -or ($missing.Count -gt 0)) { throw "unexpected user keychain search list ($($list -join ', ')); fix it before signing" }
  return , $list
}

# The Application .p12 into a temporary keychain added beside the user's keychains.
function Import-SigningIdentity([string]$WorkDir, [string]$Keychain, [string[]]$KeychainList) {
  $keychainSecret = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(24))
  $cert = Join-Path $WorkDir 'devid-application.p12'
  [IO.File]::WriteAllBytes($cert, [Convert]::FromBase64String($env:MACOS_DEVID_APP_CERT_P12_BASE64))
  try {
    security create-keychain -p $keychainSecret $Keychain
    Assert-LastExit 'security create-keychain'
    security set-keychain-settings -lut 21600 $Keychain
    Assert-LastExit 'security set-keychain-settings'
    security unlock-keychain -p $keychainSecret $Keychain
    Assert-LastExit 'security unlock-keychain'
    security import $cert -P $env:MACOS_DEVID_CERT_PASSWORD -A -t cert -f pkcs12 -k $Keychain
    Assert-LastExit 'security import devid-application.p12'
    security set-key-partition-list -S 'apple-tool:,apple:' -k $keychainSecret $Keychain | Out-Null
    Assert-LastExit 'security set-key-partition-list'
    security list-keychains -d user -s $Keychain @KeychainList
    Assert-LastExit 'security list-keychains'
  }
  finally {
    Remove-Item -LiteralPath $cert -Force -ErrorAction SilentlyContinue
  }
}

function Invoke-DeveloperIdCodesign([string]$Path, [string]$Keychain, [switch]$Runtime, [string]$Entitlements) {
  $options = @('--force', '--timestamp', '--keychain', $Keychain, '-s', $env:MACOS_SIGN_APP_IDENTITY)
  if ($Runtime) { $options += '--options', 'runtime' }
  if ($Entitlements) { $options += '--entitlements', $Entitlements }
  codesign @options $Path
  Assert-LastExit "codesign $(Split-Path -Leaf $Path)"
}

# notarytool's exit status alone does not say the submission was accepted, so its status line is
# checked too; on a rejection Apple's log for the submission is printed.
function Invoke-Notarization([string]$Path, [string]$KeyFile) {
  $name = Split-Path -Leaf $Path
  $auth = @('--key', $KeyFile, '--key-id', $env:MACOS_NOTARY_API_KEY_ID, '--issuer', $env:MACOS_NOTARY_API_ISSUER_ID)
  $lines = @(xcrun notarytool submit $Path @auth --wait 2>&1 | ForEach-Object { "$_" })
  $exit = $LASTEXITCODE
  $lines
  $text = $lines -join "`n"
  if (($exit -eq 0) -and ($text -match '(?m)^\s*status:\s*Accepted\s*$')) { return }
  if ($text -match '(?m)^\s*id:\s*([0-9a-fA-F-]{36})\s*$') {
    xcrun notarytool log $Matches[1] @auth 2>&1 | ForEach-Object { "$_" }
  }
  throw "notarization of $name was not accepted (notarytool exit $exit)"
}

# `stapler staple` can fail just after an accepted notarization, before Apple's ticket has
# propagated, so it is retried (yaat's pack-macos.ps1 retries its whole pack the same way).
function Invoke-Staple([string]$Path) {
  foreach ($attempt in 1..3) {
    xcrun stapler staple $Path
    if (($LASTEXITCODE -eq 0) -or ($attempt -eq 3)) { break }
    Write-Warning "xcrun stapler staple $(Split-Path -Leaf $Path) failed (exit $LASTEXITCODE), attempt $attempt of 3; retrying in 60 s"
    Start-Sleep -Seconds 60
  }
  Assert-LastExit "xcrun stapler staple $(Split-Path -Leaf $Path)"
}

# Gatekeeper's verdict, printed and required to hold every phrase in $Expect.
function Assert-Assessment([string]$What, [string[]]$Expect, [string[]]$Arguments) {
  $lines = @(spctl @Arguments 2>&1 | ForEach-Object { "$_" })
  $exit = $LASTEXITCODE
  $lines
  if ($exit -ne 0) { throw "spctl refused $What (exit $exit)" }
  $text = $lines -join "`n"
  foreach ($phrase in $Expect) {
    if (-not $text.Contains($phrase)) { throw "spctl's verdict on $What does not say '$phrase'" }
  }
}

$version = Get-WorkspaceVersion
$binaries = 'rustling-tulip-native', 'rustling-tulipd', 'rt-tracer'
$release = Join-Path $root 'target/release'
$pwaDist = Join-Path $root 'apps/pwa/dist'

if (-not $SkipBuild) {
  if (-not (Get-Command pnpm -ErrorAction SilentlyContinue)) { throw 'pnpm not found: the phone app needs Node and pnpm on this Mac' }
  cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p rustling-tulip-native -p daemon -p tracer
  Assert-LastExit 'cargo build --release'
  # The phone app is a bundle resource, never a Contents/MacOS file: codesign refuses non-code there.
  pnpm --dir (Join-Path $root 'apps/pwa') install --frozen-lockfile
  Assert-LastExit 'pnpm install (apps/pwa)'
  pnpm --dir (Join-Path $root 'apps/pwa') build
  Assert-LastExit 'pnpm build (apps/pwa)'
}
if (-not (Test-Path -LiteralPath (Join-Path $pwaDist 'index.html') -PathType Leaf)) {
  throw "missing $pwaDist/index.html (build the phone app, or drop -SkipBuild)"
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
/bin/cp -Rp $pwaDist (Join-Path $resources 'pwa')
Assert-LastExit 'cp apps/pwa/dist'
Build-AppIcon $staging (Join-Path $resources 'icon.icns')
Remove-Item -LiteralPath (Join-Path $staging 'icon.iconset') -Recurse -Force
Build-InfoPlist $version (Join-Path $app 'Contents/Info.plist')

$base = "rustling-tulip-$version-macos-arm64"
$zip = Join-Path $OutDir "$base.zip"
$sha = "$zip.sha256"
$dmg = Join-Path $OutDir "$base.dmg"
foreach ($old in $zip, $sha, $dmg) {
  if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Force }
}

$signingDir = $null
$keychain = $null
$originalList = $null
if ($Sign) {
  $signingDir = Join-Path ([IO.Path]::GetTempPath()) "rustling-tulip-signing-$([guid]::NewGuid().ToString('N'))"
  New-Item -ItemType Directory -Path $signingDir | Out-Null
  $keychain = Join-Path $signingDir 'signing.keychain-db'
  $originalList = Get-UserKeychainList
}
try {
  if ($Sign) {
    Import-SigningIdentity $signingDir $keychain $originalList
    $notaryKey = Join-Path $signingDir 'notary-key.p8'
    [IO.File]::WriteAllBytes($notaryKey, [Convert]::FromBase64String($env:MACOS_NOTARY_API_KEY_P8_BASE64))

    # Inside out with the hardened runtime: each binary, then the bundle (no --deep), which seals
    # Info.plist and Resources. The client and the bundle carry the Apple-events entitlement, without
    # which the osascript that opens a page at a link's #fragment is denied by macOS.
    $entitlements = Join-Path $root 'tools/installer/macos/rustling-tulip.entitlements'
    foreach ($bin in 'rt-tracer', 'rustling-tulipd') {
      Invoke-DeveloperIdCodesign (Join-Path $macOS $bin) $keychain -Runtime
    }
    Invoke-DeveloperIdCodesign (Join-Path $macOS 'rustling-tulip-native') $keychain -Runtime -Entitlements $entitlements
    Invoke-DeveloperIdCodesign $app $keychain -Runtime -Entitlements $entitlements

    # A bundle is submitted zipped; the ticket is stapled to the app itself.
    $submission = Join-Path $signingDir 'rustling-tulip-notarize.zip'
    ditto -c -k --keepParent $app $submission
    Assert-LastExit 'ditto (notarization zip)'
    Invoke-Notarization $submission $notaryKey
    Invoke-Staple $app
  }
  else {
    # Inside out: each binary, then the bundle, which seals Info.plist and Resources.
    foreach ($bin in $binaries) {
      codesign --force --sign - (Join-Path $macOS $bin)
      Assert-LastExit "codesign $bin"
    }
    codesign --force --sign - $app
    Assert-LastExit 'codesign rustling-tulip.app'
    codesign --verify --deep --strict --verbose=2 $app
    Assert-LastExit 'codesign --verify rustling-tulip.app'
  }

  ditto -c -k --keepParent $app $zip
  Assert-LastExit 'ditto'
  $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
  [IO.File]::WriteAllText($sha, "$hash  $(Split-Path -Leaf $zip)`n")

  ln -s /Applications (Join-Path $dmgRoot 'Applications')
  Assert-LastExit 'ln -s /Applications'
  hdiutil create -volname rustling-tulip -srcfolder $dmgRoot -ov -format UDZO $dmg
  Assert-LastExit 'hdiutil create'

  if ($Sign) {
    Invoke-DeveloperIdCodesign $dmg $keychain
    Invoke-Notarization $dmg $notaryKey
    Invoke-Staple $dmg

    codesign --verify --deep --strict --verbose=2 $app
    Assert-LastExit 'codesign --verify rustling-tulip.app'
    Assert-Assessment 'rustling-tulip.app' @('accepted', 'source=Notarized Developer ID') @('--assess', '-vvv', '--type', 'execute', $app)
    Assert-Assessment (Split-Path -Leaf $dmg) @('accepted') @('--assess', '-vvv', '--type', 'open', '--context', 'context:primary-signature', $dmg)
    xcrun stapler validate $app
    Assert-LastExit 'xcrun stapler validate rustling-tulip.app'
    xcrun stapler validate $dmg
    Assert-LastExit "xcrun stapler validate $(Split-Path -Leaf $dmg)"
  }
}
finally {
  if ($Sign) {
    security list-keychains -d user -s @originalList
    if (Test-Path -LiteralPath $keychain) { security delete-keychain $keychain }
    Remove-Item -LiteralPath $signingDir -Recurse -Force -ErrorAction SilentlyContinue
    $restoredList = Get-UserKeychainList
    if (Compare-Object $originalList $restoredList) { Write-Warning "the user keychain search list differs from before the run: $($restoredList -join ', ')" }
  }
}

$dmg
$zip
$sha
