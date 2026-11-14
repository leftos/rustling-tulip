#-------------------------------------------------------------------------------
# tools/ios/build.ps1 -- build the phone app (apps/ios) for an iOS target and
# assemble it into a flat app bundle under target/ios/<target>/.
#
# Runs on the Mac (pwsh). The bundle holds the binary and apps/ios/Info.plist,
# its version keys set from the workspace version. A simulator bundle is signed
# ad hoc; a device bundle is left unsigned unless -Sign is given.
#
#   pwsh -NoProfile -File tools/ios/build.ps1 [-Target sim|device] [-Release]
#   pwsh -NoProfile -File tools/ios/build.ps1 -Target device -Sign -BuildNumber <n>
#
# -Sign (device only; it forces the release build) signs the bundle for the App
# Store and writes target/ios/aarch64-apple-ios/RustlingTulip.ipa: it sets
# CFBundleVersion to -BuildNumber, stamps the build-machine keys, compiles the
# app icon, embeds the provisioning profile, codesigns with the distribution
# identity from a temporary keychain and verifies the signature. It needs
# IOS_DIST_CERT_P12_BASE64, IOS_DIST_CERT_PASSWORD_BASE64, IOS_PHONE_PROFILE_BASE64 and
# IOS_TEAM_ID (tools/installer/README.md "iOS signing"), which it moves out of
# the environment before the build runs. No signing value is put on a command
# line except the .p12 password, which `security import -P` takes as an
# argument, as the macOS signing path does.
#
# Prints the bundle path last, or with -Sign the .ipa path.
#-------------------------------------------------------------------------------
[CmdletBinding()]
param(
    [ValidateSet('sim', 'device')]
    [string] $Target = 'sim',
    [switch] $Release,
    [switch] $Sign,
    [int64] $BuildNumber
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$BinaryName = 'rustling-tulip-ios'
$PlistBuddy = '/usr/libexec/PlistBuddy'
$AppId = 'dev.leftos.rustling-tulip.phone'
$SigningNames = 'IOS_DIST_CERT_P12_BASE64', 'IOS_DIST_CERT_PASSWORD_BASE64', 'IOS_PHONE_PROFILE_BASE64', 'IOS_TEAM_ID'
# -Sign's variables, moved out of the environment before the build (see the end of this script).
$Signing = @{}

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

function Write-PlistValue([string] $Plist, [string] $Key, [string] $Value) {
    & $PlistBuddy -c "Set :$Key $Value" $Plist
    Assert-LastExit "PlistBuddy Set :$Key"
}

function Build-AppBundle([string] $Triple) {
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
    Write-PlistValue $plist 'CFBundleShortVersionString' $numeric
    Write-PlistValue $plist 'CFBundleVersion' $numeric
    return $app
}

# Sets a top-level string key, adding it when the plist lacks it.
function Write-PlistString([string] $Plist, [string] $Key, [string] $Value) {
    & $PlistBuddy -c "Print :$Key" $Plist 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) {
        & $PlistBuddy -c "Set :$Key $Value" $Plist
        Assert-LastExit "PlistBuddy Set :$Key"
    }
    else {
        & $PlistBuddy -c "Add :$Key string $Value" $Plist
        Assert-LastExit "PlistBuddy Add :$Key"
    }
}

function Get-ToolOutput([string] $What, [string[]] $Command) {
    $exe = $Command[0]
    $rest = @($Command | Select-Object -Skip 1)
    $out = @(& $exe @rest)
    Assert-LastExit $What
    return (($out | ForEach-Object { "$_" }) -join "`n").Trim()
}

# Xcode writes DTXcode as four digits: 26.1 -> 2610, 16.4.1 -> 1641.
function ConvertTo-DTXcode([string] $Version) {
    $parts = @($Version.Split('.') | ForEach-Object { [int] $_ })
    while ($parts.Count -lt 3) { $parts += 0 }
    return ('{0:D2}{1}{2}' -f $parts[0], $parts[1], $parts[2])
}

# The keys Xcode writes for an App Store build, from this Mac's own tools.
function Write-BuildMachineKey([string] $Plist) {
    $sdkVersion = Get-ToolOutput 'xcrun --show-sdk-version' @('xcrun', '--sdk', 'iphoneos', '--show-sdk-version')
    $sdkBuild = Get-ToolOutput 'xcrun --show-sdk-build-version' @('xcrun', '--sdk', 'iphoneos', '--show-sdk-build-version')
    $xcode = Get-ToolOutput 'xcodebuild -version' @('xcodebuild', '-version')
    if ($xcode -notmatch '(?m)^Xcode\s+([0-9.]+)\s*$') { throw "no Xcode version in xcodebuild -version: $xcode" }
    $xcodeVersion = $Matches[1]
    if ($xcode -notmatch '(?m)^Build version\s+(\S+)\s*$') { throw "no build version in xcodebuild -version: $xcode" }
    $xcodeBuild = $Matches[1]
    $osBuild = Get-ToolOutput 'sw_vers -buildVersion' @('sw_vers', '-buildVersion')
    $keys = [ordered]@{
        DTPlatformName      = 'iphoneos'
        DTPlatformVersion   = $sdkVersion
        DTSDKName           = "iphoneos$sdkVersion"
        DTSDKBuild          = $sdkBuild
        DTPlatformBuild     = $sdkBuild
        DTXcode             = ConvertTo-DTXcode $xcodeVersion
        DTXcodeBuild        = $xcodeBuild
        DTCompiler          = 'com.apple.compilers.llvm.clang.1_0'
        BuildMachineOSBuild = $osBuild
    }
    foreach ($key in $keys.Keys) { Write-PlistString $Plist $key $keys[$key] }
}

# One universal 1024x1024 icon in a generated asset catalog, compiled into the
# bundle's Assets.car; actool's partial plist is merged into Info.plist.
function Build-AppIcon([string] $App, [string] $WorkDir) {
    $source = Join-Path $RepoRoot 'tools/installer/icon/icon-1024.png'
    $catalog = Join-Path $WorkDir 'Assets.xcassets'
    $iconSet = Join-Path $catalog 'AppIcon.appiconset'
    New-Item -ItemType Directory -Force -Path $iconSet | Out-Null
    [IO.File]::WriteAllText((Join-Path $catalog 'Contents.json'), "{`n  `"info`" : { `"author`" : `"xcode`", `"version`" : 1 }`n}`n")
    $contents = @'
{
  "images" : [
    { "filename" : "icon-1024.png", "idiom" : "universal", "platform" : "ios", "size" : "1024x1024" }
  ],
  "info" : { "author" : "xcode", "version" : 1 }
}
'@
    [IO.File]::WriteAllText((Join-Path $iconSet 'Contents.json'), $contents.Replace("`r`n", "`n") + "`n")
    $icon = Join-Path $iconSet 'icon-1024.png'
    $alpha = Get-ToolOutput 'sips -g hasAlpha' @('sips', '-g', 'hasAlpha', $source)
    if ($alpha -match 'hasAlpha:\s*yes') {
        # The App Store refuses an icon with an alpha channel: JPEG has none.
        $flat = Join-Path $WorkDir 'icon-flat.jpg'
        & sips -s format jpeg $source --out $flat | Out-Host
        Assert-LastExit 'sips -s format jpeg'
        & sips -s format png $flat --out $icon | Out-Host
        Assert-LastExit 'sips -s format png'
    }
    else {
        Copy-Item -LiteralPath $source -Destination $icon
    }
    $partial = Join-Path $WorkDir 'icon.plist'
    & xcrun actool --compile $App --platform iphoneos --minimum-deployment-target 15.0 --app-icon AppIcon `
        --output-partial-info-plist $partial $catalog | Out-Host
    Assert-LastExit 'xcrun actool'
    if (-not (Test-Path -LiteralPath (Join-Path $App 'Assets.car'))) { throw 'xcrun actool wrote no Assets.car into the bundle' }
    if (-not (Test-Path -LiteralPath $partial)) { throw 'xcrun actool wrote no partial Info.plist' }
    & $PlistBuddy -c "Merge $partial" (Join-Path $App 'Info.plist') | Out-Host
    Assert-LastExit 'PlistBuddy Merge icon.plist'
}

function Get-PlistEntry([string] $Plist, [string] $Key) {
    $out = @(& $PlistBuddy -c "Print :$Key" $Plist 2>$null)
    if ($LASTEXITCODE -ne 0) { return $null }
    return (($out | ForEach-Object { "$_" }) -join "`n").Trim()
}

# The profile's own entitlements must allow what the app is signed with.
function Test-ProfileEntitlement([string] $ProfilePath, [string] $WorkDir) {
    $decoded = Join-Path $WorkDir 'profile.plist'
    & security cms -D -i $ProfilePath -o $decoded
    Assert-LastExit 'security cms -D'
    if ((Get-PlistEntry $decoded 'Entitlements:application-identifier') -ne "$($Signing['IOS_TEAM_ID']).$AppId") {
        throw "the provisioning profile's application-identifier does not match $AppId on the team in IOS_TEAM_ID"
    }
    if ((Get-PlistEntry $decoded 'Entitlements:aps-environment') -ne 'production') {
        throw "the provisioning profile's aps-environment is not production"
    }
    $beta = Get-PlistEntry $decoded 'Entitlements:beta-reports-active'
    if ($null -eq $beta) { throw "the provisioning profile has no beta-reports-active entitlement" }
    if ($beta -ne 'true') { throw "the provisioning profile's beta-reports-active is not true" }
}

function Write-Entitlement([string] $Destination) {
    $template = Get-Content -LiteralPath (Join-Path $RepoRoot 'apps/ios/app.entitlements') -Raw
    [IO.File]::WriteAllText($Destination, $template.Replace('__TEAM_ID__', $Signing['IOS_TEAM_ID']).Replace("`r`n", "`n"))
}

# Always an array, even of one keychain: a lone string splatted into `security list-keychains -s`
# would pass each character as a keychain path and wreck the user's search list (yaat's pack-macos.ps1).
function Get-UserKeychainList {
    $list = @(security list-keychains -d user | ForEach-Object { $_.Trim().Trim('"') } | Where-Object { $_ })
    $missing = @($list | Where-Object { -not (Test-Path -LiteralPath $_ -PathType Leaf) })
    if (($list.Count -eq 0) -or ($missing.Count -gt 0)) { throw "unexpected user keychain search list ($($list -join ', ')); fix it before signing" }
    return , $list
}

# The distribution .p12 into a temporary keychain added beside the user's keychains.
function Import-SigningIdentity([string] $WorkDir, [string] $Keychain, [string[]] $KeychainList) {
    $keychainSecret = [Convert]::ToBase64String([Security.Cryptography.RandomNumberGenerator]::GetBytes(24))
    $cert = Join-Path $WorkDir 'apple-distribution.p12'
    [IO.File]::WriteAllBytes($cert, [Convert]::FromBase64String($Signing['IOS_DIST_CERT_P12_BASE64']))
    $certPassword = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Signing['IOS_DIST_CERT_PASSWORD_BASE64']))
    try {
        security create-keychain -p $keychainSecret $Keychain
        Assert-LastExit 'security create-keychain'
        security set-keychain-settings -lut 21600 $Keychain
        Assert-LastExit 'security set-keychain-settings'
        security unlock-keychain -p $keychainSecret $Keychain
        Assert-LastExit 'security unlock-keychain'
        security import $cert -P $certPassword -A -t cert -f pkcs12 -k $Keychain | Out-Host
        Assert-LastExit 'security import apple-distribution.p12'
        security set-key-partition-list -S 'apple-tool:,apple:' -k $keychainSecret $Keychain | Out-Null
        Assert-LastExit 'security set-key-partition-list'
        security list-keychains -d user -s $Keychain @KeychainList
        Assert-LastExit 'security list-keychains'
    }
    finally {
        Remove-Item -LiteralPath $cert -Force -ErrorAction SilentlyContinue
    }
}

# An Apple Distribution certificate chains through Apple's WWDR G3 intermediate, which a Mac that
# never signed in to Xcode lacks; without it the identity is not trusted for codesigning. The
# intermediate is public and goes into the temporary keychain alone.
function Import-WwdrIntermediate([string] $WorkDir, [string] $Keychain) {
    $file = Join-Path $WorkDir 'AppleWWDRCAG3.cer'
    $message = @(& curl -fsSL --max-time 30 -o $file 'https://www.apple.com/certificateauthority/AppleWWDRCAG3.cer' 2>&1 | ForEach-Object { "$_" })
    if ($LASTEXITCODE -ne 0) { throw "testflight: could not fetch Apple's WWDR G3 intermediate: $($message -join ' ')" }
    security import $file -k $Keychain | Out-Host
    Assert-LastExit 'security import AppleWWDRCAG3.cer'
}

function Get-SigningIdentity([string] $Keychain) {
    $lines = @(security find-identity -v -p codesigning $Keychain | ForEach-Object { "$_" })
    Assert-LastExit 'security find-identity'
    $ids = @($lines | Where-Object { $_ -match '^\s*\d+\)\s+([0-9A-Fa-f]{40})\s' } | ForEach-Object { ($_ -split '\s+' | Where-Object { $_ })[1] })
    if ($ids.Count -ne 1) { throw "expected one valid codesigning identity in the temporary keychain, found $($ids.Count)" }
    return $ids[0]
}

function Invoke-AppStoreSigning([string] $App) {
    $plist = Join-Path $App 'Info.plist'
    Write-PlistValue $plist 'CFBundleVersion' "$BuildNumber"
    Write-BuildMachineKey $plist

    $workDir = Join-Path ([IO.Path]::GetTempPath()) "rustling-tulip-ios-signing-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $workDir | Out-Null
    $keychain = Join-Path $workDir 'signing.keychain-db'
    $originalList = Get-UserKeychainList
    try {
        Build-AppIcon $App $workDir
        # The profile stays in the work folder until it is checked; it ships inside the signed app.
        $profilePath = Join-Path $workDir 'phone-appstore.mobileprovision'
        [IO.File]::WriteAllBytes($profilePath, [Convert]::FromBase64String($Signing['IOS_PHONE_PROFILE_BASE64']))
        $entitlements = Join-Path $workDir 'app.entitlements'
        Write-Entitlement $entitlements
        Test-ProfileEntitlement $profilePath $workDir

        Import-SigningIdentity $workDir $keychain $originalList
        Import-WwdrIntermediate $workDir $keychain
        $identity = Get-SigningIdentity $keychain
        Copy-Item -LiteralPath $profilePath -Destination (Join-Path $App 'embedded.mobileprovision')
        codesign --force --sign $identity --keychain $keychain --entitlements $entitlements $App
        Assert-LastExit 'codesign RustlingTulip.app'
        codesign --verify --strict --verbose=2 $App
        Assert-LastExit 'codesign --verify RustlingTulip.app'
        Write-Output 'codesign --verify --strict passed'

        $payload = Join-Path $workDir 'Payload'
        New-Item -ItemType Directory -Path $payload | Out-Null
        ditto $App (Join-Path $payload 'RustlingTulip.app')
        Assert-LastExit 'ditto (Payload)'
        $ipa = Join-Path (Split-Path -Parent $App) 'RustlingTulip.ipa'
        if (Test-Path -LiteralPath $ipa) { Remove-Item -LiteralPath $ipa -Force }
        ditto -c -k --keepParent $payload $ipa
        Assert-LastExit 'ditto (ipa)'
    }
    finally {
        security list-keychains -d user -s @originalList
        if (Test-Path -LiteralPath $keychain) { security delete-keychain $keychain }
        Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
        $restoredList = Get-UserKeychainList
        if (Compare-Object $originalList $restoredList) { Write-Warning "the user keychain search list differs from before the run: $($restoredList -join ', ')" }
    }
    Write-Output $ipa
}

if (-not $IsMacOS) { throw 'tools/ios/build.ps1 runs on the Mac: iOS builds need Xcode' }
if ($Sign) {
    if ($Target -ne 'device') { throw '-Sign signs a device build: give -Target device' }
    if (-not $PSBoundParameters.ContainsKey('BuildNumber') -or $BuildNumber -le 0) { throw '-Sign needs -BuildNumber <n> (a positive integer)' }
    $Release = [switch] $true
    # Held in this script alone: rustup, cargo and the build scripts they start never see them.
    foreach ($name in $SigningNames) {
        $value = [Environment]::GetEnvironmentVariable($name)
        if (-not $value) { throw "-Sign needs the environment variable $name, which is not set" }
        $Signing[$name] = $value
        [Environment]::SetEnvironmentVariable($name, $null)
    }
}

$triple = if ($Target -eq 'sim') { 'aarch64-apple-ios-sim' } else { 'aarch64-apple-ios' }
Invoke-CargoBuild $triple
$app = Build-AppBundle $triple
if ($Target -eq 'sim') {
    & codesign --force --sign - $app
    Assert-LastExit 'codesign'
}
elseif ($Sign) {
    Invoke-AppStoreSigning $app
    return
}
else {
    Write-Output "unsigned device bundle at $app; sign it with -Sign -BuildNumber <n>"
}
Write-Output $app
