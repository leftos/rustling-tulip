#-------------------------------------------------------------------------------
# tools/ios/build.ps1 -- build the phone app (apps/ios) for an iOS target and
# assemble it into a flat app bundle under target/ios/<target>/.
#
# Runs on the Mac (pwsh). The bundle holds the binary and apps/ios/Info.plist,
# its version keys set from the workspace version, and the notification service
# extension (apps/ios-notify) as PlugIns/RustlingTulipNotify.appex with the same
# version keys, and the fallback alert texts as en.lproj/Localizable.strings. A
# simulator bundle is signed ad hoc, the extension first; a device bundle is
# left unsigned unless -Sign is given.
#
#   pwsh -NoProfile -File tools/ios/build.ps1 [-Target sim|device] [-Release]
#   pwsh -NoProfile -File tools/ios/build.ps1 -Target device -Sign -BuildNumber <n>
#
# -Sign (device only; it forces the release build) signs the bundle for the App
# Store and writes target/ios/aarch64-apple-ios/RustlingTulip.ipa: it sets
# CFBundleVersion to -BuildNumber and stamps the build-machine keys in the app
# and the extension, compiles the app icon, checks both provisioning profiles,
# embeds each in its bundle, codesigns the extension with its own entitlements
# and then the app, with the distribution identity from a temporary keychain,
# and verifies each signature. It needs IOS_DIST_CERT_P12_BASE64,
# IOS_DIST_CERT_PASSWORD_BASE64, IOS_PHONE_PROFILE_GZ_BASE64, IOS_NOTIFY_PROFILE_GZ_BASE64
# (each profile gzipped, then base64) and IOS_TEAM_ID (tools/installer/README.md "iOS signing"), which it moves out of
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
$NotifyBinaryName = 'rustling-tulip-ios-notify'
$NotifyBundle = 'PlugIns/RustlingTulipNotify.appex'
# App Store Connect refuses an extension whose executable is not named after its bundle folder.
$NotifyExecutable = 'RustlingTulipNotify'
$PlistBuddy = '/usr/libexec/PlistBuddy'
$AppId = 'dev.leftos.rustling-tulip.phone'
$SigningNames = 'IOS_DIST_CERT_P12_BASE64', 'IOS_DIST_CERT_PASSWORD_BASE64', 'IOS_PHONE_PROFILE_GZ_BASE64', 'IOS_NOTIFY_PROFILE_GZ_BASE64', 'IOS_TEAM_ID'
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
    $cargoArgs = @('build', '-p', $BinaryName, '-p', $NotifyBinaryName, '--target', $Triple)
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
    # The relay's alert names loc keys, which iOS resolves from the app bundle when the extension leaves a push as it came.
    $lproj = Join-Path $app 'en.lproj'
    New-Item -ItemType Directory -Force -Path $lproj | Out-Null
    $strings = Join-Path $lproj 'Localizable.strings'
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'apps/ios/en.lproj/Localizable.strings') -Destination $strings
    & plutil -lint $strings | Out-Host
    Assert-LastExit 'plutil -lint en.lproj/Localizable.strings'
    Get-ChildItem -LiteralPath $lproj -Name | ForEach-Object { "en.lproj: $_" } | Out-Host
    $numeric = ((Get-WorkspaceVersion) -split '-')[0]
    Write-PlistValue $plist 'CFBundleShortVersionString' $numeric
    Write-PlistValue $plist 'CFBundleVersion' $numeric
    Build-NotifyExtension $app (Join-Path $RepoRoot "target/$Triple/$profileDir/$NotifyBinaryName") $numeric
    return $app
}

# The notification service extension, PlugIns/RustlingTulipNotify.appex: its binary and
# apps/ios-notify/Info.plist, carrying the app's version keys.
function Build-NotifyExtension([string] $App, [string] $Binary, [string] $Version) {
    if (-not (Test-Path $Binary)) { throw "no extension binary at $Binary after the build" }
    $appex = Join-Path $App $NotifyBundle
    New-Item -ItemType Directory -Force -Path $appex | Out-Null
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $appex $NotifyExecutable)
    $plist = Join-Path $appex 'Info.plist'
    Copy-Item -LiteralPath (Join-Path $RepoRoot 'apps/ios-notify/Info.plist') -Destination $plist
    Write-PlistValue $plist 'CFBundleShortVersionString' $Version
    Write-PlistValue $plist 'CFBundleVersion' $Version
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

# The extension's profile must allow its own application-identifier and the keychain group it shares
# with the app (an exact entry, or a wildcard such as TEAM.* that covers it).
function Test-NotifyProfileEntitlement([string] $ProfilePath, [string] $WorkDir) {
    $decoded = Join-Path $WorkDir 'notify-profile.plist'
    & security cms -D -i $ProfilePath -o $decoded
    Assert-LastExit 'security cms -D (notify profile)'
    $team = $Signing['IOS_TEAM_ID']
    if ((Get-PlistEntry $decoded 'Entitlements:application-identifier') -ne "$team.$AppId.notify") {
        throw "the extension's provisioning profile's application-identifier does not match $AppId.notify on the team in IOS_TEAM_ID"
    }
    $group = "$team.dev.leftos.rustling-tulip"
    $raw = Get-PlistEntry $decoded 'Entitlements:keychain-access-groups'
    $groups = @(if ($raw) { $raw -split "`n" | ForEach-Object { $_.Trim() } | Where-Object { $_ -and ($_ -ne 'Array {') -and ($_ -ne '}') } })
    $covering = @($groups | Where-Object { ($_ -eq $group) -or ($_.EndsWith('*') -and $group.StartsWith($_.TrimEnd('*'))) })
    if ($covering.Count -eq 0) { throw "the extension's provisioning profile's keychain-access-groups do not cover the group it shares with the app" }
    if ((Get-PlistEntry $decoded 'Entitlements:beta-reports-active') -ne 'true') {
        throw "the extension's provisioning profile does not allow beta-reports-active"
    }
}

# A signing variable that holds a file gzipped, then base64 (testflight.ps1 packs each profile so),
# written out as the file's own bytes. The error names the variable, never its value.
function Write-GzipBase64File([string] $Name, [string] $Destination) {
    try {
        $source = [IO.MemoryStream]::new([Convert]::FromBase64String($Signing[$Name]))
        $gzip = [IO.Compression.GZipStream]::new($source, [IO.Compression.CompressionMode]::Decompress)
        $plain = [IO.MemoryStream]::new()
        try { $gzip.CopyTo($plain) } finally { $gzip.Dispose() }
    }
    catch {
        throw "$Name is not gzip-compressed base64"
    }
    [IO.File]::WriteAllBytes($Destination, $plain.ToArray())
}

# A repo entitlements template with the team filled in, LF-terminated.
function Write-Entitlement([string] $Template, [string] $Destination) {
    $text = Get-Content -LiteralPath (Join-Path $RepoRoot $Template) -Raw
    [IO.File]::WriteAllText($Destination, $text.Replace('__TEAM_ID__', $Signing['IOS_TEAM_ID']).Replace("`r`n", "`n"))
}

# Inside out: the extension is signed with its own entitlements before the app's signature seals it.
function Set-ExtensionSignature([string] $Appex, [string] $Identity, [string] $Keychain, [string] $Entitlements) {
    codesign --force --sign $Identity --keychain $Keychain --entitlements $Entitlements $Appex
    Assert-LastExit 'codesign RustlingTulipNotify.appex'
    codesign --verify --strict --verbose=2 $Appex
    Assert-LastExit 'codesign --verify RustlingTulipNotify.appex'
    Write-Output 'codesign --verify --strict passed for RustlingTulipNotify.appex'
    $signed = @(codesign -d --entitlements :- $Appex 2>$null | ForEach-Object { "$_" })
    Assert-LastExit 'codesign -d --entitlements RustlingTulipNotify.appex'
    $keys = @([regex]::Matches(($signed -join ''), '<key>([^<]+)</key>') | ForEach-Object { $_.Groups[1].Value })
    Write-Output "RustlingTulipNotify.appex entitlement keys: $($keys -join ', ')"
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
    $appex = Join-Path $App $NotifyBundle
    # App and extension carry the same build number; every plist edit lands before a signature seals it.
    foreach ($plist in (Join-Path $App 'Info.plist'), (Join-Path $appex 'Info.plist')) {
        Write-PlistValue $plist 'CFBundleVersion' "$BuildNumber"
        Write-BuildMachineKey $plist
    }

    $workDir = Join-Path ([IO.Path]::GetTempPath()) "rustling-tulip-ios-signing-$([guid]::NewGuid().ToString('N'))"
    New-Item -ItemType Directory -Path $workDir | Out-Null
    $keychain = Join-Path $workDir 'signing.keychain-db'
    $originalList = Get-UserKeychainList
    try {
        Build-AppIcon $App $workDir
        # The profile stays in the work folder until it is checked; it ships inside the signed app.
        $profilePath = Join-Path $workDir 'phone-appstore.mobileprovision'
        Write-GzipBase64File 'IOS_PHONE_PROFILE_GZ_BASE64' $profilePath
        $entitlements = Join-Path $workDir 'app.entitlements'
        Write-Entitlement 'apps/ios/app.entitlements' $entitlements
        Test-ProfileEntitlement $profilePath $workDir
        $notifyProfile = Join-Path $workDir 'notify-appstore.mobileprovision'
        Write-GzipBase64File 'IOS_NOTIFY_PROFILE_GZ_BASE64' $notifyProfile
        $notifyEntitlements = Join-Path $workDir 'notify.entitlements'
        Write-Entitlement 'apps/ios-notify/notify.entitlements' $notifyEntitlements
        Test-NotifyProfileEntitlement $notifyProfile $workDir

        Import-SigningIdentity $workDir $keychain $originalList
        Import-WwdrIntermediate $workDir $keychain
        $identity = Get-SigningIdentity $keychain
        Copy-Item -LiteralPath $notifyProfile -Destination (Join-Path $appex 'embedded.mobileprovision')
        Copy-Item -LiteralPath $profilePath -Destination (Join-Path $App 'embedded.mobileprovision')
        Set-ExtensionSignature $appex $identity $keychain $notifyEntitlements
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
    # Inside out: the extension first, then the app that holds it.
    & codesign --force --sign - (Join-Path $app $NotifyBundle)
    Assert-LastExit 'codesign RustlingTulipNotify.appex'
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
