#-------------------------------------------------------------------------------
# tools/ios/testflight.ps1 -- build the phone app for a real iPhone, sign it for
# the App Store and upload it to App Store Connect, where it reaches TestFlight.
#
# On this PC (no -OnMac) it reads the owner's iOS signing set from Private
# Config on G: into this process's environment alone, then runs itself with
# -OnMac as one pinned heavy gate run on the Mac mini builder (log
# .tmp/ios-testflight.log). The variables reach the Mac as bare --env names: no
# value is in a file in the repo or in a log, nor on a command line except the
# .p12 password, which `security import -P` takes as an argument on the Mac (as
# the macOS signing path does).
#
# On the Mac (-OnMac) it checks Xcode's tools, moves the App Store Connect key
# variables out of its environment, runs
# `tools/ios/build.ps1 -Target device -Sign -BuildNumber <n>` and uploads the
# .ipa with `xcrun altool --upload-app`, the key decoded into the run's temp
# folder and found through API_PRIVATE_KEYS_DIR. The upload counts only when
# altool exits 0 and says UPLOAD SUCCEEDED. -NoUpload stops after the signed
# build.
#
#   pwsh -NoProfile -File tools/ios/testflight.ps1 [-NoUpload]
#
# Prints `testflight: uploaded build <n>`, or with -NoUpload
# `testflight: signed build <n> at <ipa>`, last.
#-------------------------------------------------------------------------------
[CmdletBinding()]
param(
    [switch] $OnMac,
    [switch] $NoUpload
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$MacBuilder = 'builder@mac-mini'
$SigningDir = 'G:\My Drive\Private Config\rustling-tulip\secrets\ios-signing'
$Names = 'IOS_DIST_CERT_P12_BASE64', 'IOS_DIST_CERT_PASSWORD_BASE64', 'IOS_PHONE_PROFILE_BASE64', 'IOS_ASC_API_KEY_P8_BASE64',
'IOS_TEAM_ID', 'IOS_ASC_API_KEY_ID', 'IOS_ASC_API_ISSUER_ID', 'IOS_BUILD_NUMBER'

function Assert-LastExit([string] $Command) {
    if ($LASTEXITCODE -ne 0) { throw "$Command failed (exit $LASTEXITCODE)" }
}

# Reads the five files of the signing set it needs, and nothing else in the folder.
function Read-SigningSet {
    if (-not (Test-Path -LiteralPath $SigningDir -PathType Container)) { throw "no signing folder at $SigningDir (is Google Drive mounted as G:?)" }
    foreach ($file in 'apple-distribution.p12', 'p12-password.txt', 'phone-appstore.mobileprovision', 'ids.env') {
        if (-not (Test-Path -LiteralPath (Join-Path $SigningDir $file) -PathType Leaf)) { throw "missing $file in $SigningDir" }
    }
    $p8 = @(Get-ChildItem -LiteralPath $SigningDir -Filter 'AuthKey_*.p8' -File)
    if ($p8.Count -ne 1) { throw "expected one AuthKey_*.p8 in $SigningDir, found $($p8.Count)" }

    $env:IOS_DIST_CERT_P12_BASE64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $SigningDir 'apple-distribution.p12')))
    # The password is the file's bytes less a trailing newline, nothing else trimmed; base64 carries
    # any byte (a carriage return included) through --env and ssh unchanged.
    $password = [IO.File]::ReadAllBytes((Join-Path $SigningDir 'p12-password.txt'))
    $end = $password.Length
    if (($end -gt 0) -and ($password[$end - 1] -eq 10)) {
        $end--
        if (($end -gt 0) -and ($password[$end - 1] -eq 13)) { $end-- }
    }
    $env:IOS_DIST_CERT_PASSWORD_BASE64 = [Convert]::ToBase64String($password, 0, $end)
    $env:IOS_PHONE_PROFILE_BASE64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $SigningDir 'phone-appstore.mobileprovision')))
    $env:IOS_ASC_API_KEY_P8_BASE64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($p8[0].FullName))
    foreach ($line in Get-Content -LiteralPath (Join-Path $SigningDir 'ids.env')) {
        if ($line -match '^\s*(TEAM_ID|ASC_API_KEY_ID|ASC_API_ISSUER_ID)\s*=\s*(.+?)\s*$') {
            $key = $Matches[1]
            $value = $Matches[2]
            if ($value -match '^"(.*)"$' -or $value -match "^'(.*)'$") { $value = $Matches[1] }
            [Environment]::SetEnvironmentVariable("IOS_$key", $value)
        }
    }
    $env:IOS_BUILD_NUMBER = [DateTime]::UtcNow.ToString('yyyyMMddHHmm')
    foreach ($name in $Names) {
        if (-not [Environment]::GetEnvironmentVariable($name)) { throw "$name is empty after reading $SigningDir (check ids.env)" }
    }
}

function Invoke-FromPc([switch] $SkipUpload) {
    # A nested gate call would queue behind the slot this run already holds.
    if ($env:GATE_SLOT_HELD -or $env:GATE_BOX_JOB) {
        throw 'tools/ios/testflight.ps1: run it from this PC, not inside a gate job'
    }
    if (-not $IsWindows) { throw 'tools/ios/testflight.ps1 runs on this PC, which holds the signing set; the Mac side is -OnMac' }
    Read-SigningSet
    # Names only on the gate's command line; the gate reads each value from this process's environment.
    $gateArguments = @('run', '--builder', $MacBuilder, '--cd', $RepoRoot.Replace('\', '/'), '--log', '.tmp/ios-testflight.log',
        '--timeout', '3600', '--slot', 'heavy')
    foreach ($name in $Names) { $gateArguments += '--env', $name }
    $gateArguments += '--', 'pwsh', '-NoProfile', '-File', 'tools/ios/testflight.ps1', '-OnMac'
    if ($SkipUpload) { $gateArguments += '-NoUpload' }
    & pwsh -NoProfile -File (Join-Path $RepoRoot 'tools/gate.ps1') @gateArguments
    exit $LASTEXITCODE
}

# Xcode's tools this run needs, checked before any build or keychain work.
function Test-XcodeTool([switch] $SkipUpload) {
    $xcode = @(& xcodebuild -version | ForEach-Object { "$_" })
    Assert-LastExit 'xcodebuild -version'
    $sdk = (@(& xcrun --sdk iphoneos --show-sdk-version | ForEach-Object { "$_" }) -join '').Trim()
    Assert-LastExit 'xcrun --sdk iphoneos --show-sdk-version'
    $version = (($xcode | Select-Object -First 1) -replace '^Xcode\s+', '').Trim()
    Write-Output "testflight: Xcode $version, iPhoneOS SDK $sdk"
    & xcrun --find actool | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'testflight: actool is missing from this Xcode' }
    if (-not $SkipUpload) {
        $help = (@(& xcrun altool --help 2>&1 | ForEach-Object { "$_" }) -join "`n")
        if ($help -notmatch '--upload-app') { throw "testflight: this Xcode's altool cannot upload builds (no --upload-app); the upload needs Transporter" }
    }
}

# altool finds the key by its name, AuthKey_<id>.p8, in the folder API_PRIVATE_KEYS_DIR names: a
# folder of mode 700 in this run's temp folder, the key created with mode 600, deleted after.
# altool's exit status alone does not say the upload succeeded, so its output is checked too.
function Invoke-Upload([string] $Ipa, [hashtable] $Key) {
    $workDir = Join-Path ([IO.Path]::GetTempPath()) "rustling-tulip-ios-upload-$([guid]::NewGuid().ToString('N'))"
    $keyDir = Join-Path $workDir 'api-keys'
    $keyFile = Join-Path $keyDir "AuthKey_$($Key.Id).p8"
    [IO.Directory]::CreateDirectory($workDir, [IO.UnixFileMode] 'UserRead, UserWrite, UserExecute') | Out-Null
    try {
        [IO.Directory]::CreateDirectory($keyDir, [IO.UnixFileMode] 'UserRead, UserWrite, UserExecute') | Out-Null
        $options = [IO.FileStreamOptions]::new()
        $options.Mode = [IO.FileMode]::CreateNew
        $options.Access = [IO.FileAccess]::Write
        $options.UnixCreateMode = [IO.UnixFileMode] 'UserRead, UserWrite'
        $bytes = [Convert]::FromBase64String($Key.P8)
        $stream = [IO.FileStream]::new($keyFile, $options)
        try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
        $env:API_PRIVATE_KEYS_DIR = $keyDir
        try {
            $lines = @(& xcrun altool --upload-app --type ios -f $Ipa --apiKey $Key.Id --apiIssuer $Key.Issuer 2>&1 | ForEach-Object { "$_" })
            $code = $LASTEXITCODE
        }
        finally {
            $env:API_PRIVATE_KEYS_DIR = $null
        }
    }
    finally {
        if (Test-Path -LiteralPath $keyFile) { Remove-Item -LiteralPath $keyFile -Force -ErrorAction Stop }
        Remove-Item -LiteralPath $workDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    $lines | Out-Host
    if (($code -eq 0) -and (($lines -join "`n") -match 'UPLOAD SUCCEEDED')) { return }
    throw "testflight: the upload did not succeed; altool said: $(($lines | Select-Object -Last 20) -join "`n")"
}

function Invoke-OnMac([switch] $SkipUpload) {
    foreach ($name in $Names) {
        if (-not [Environment]::GetEnvironmentVariable($name)) { throw "tools/ios/testflight.ps1 -OnMac needs the environment variable $name, which is not set" }
    }
    Test-XcodeTool -SkipUpload:$SkipUpload
    # The upload's key is this script's alone: build.ps1 and the build it starts never see it.
    $key = @{ P8 = $env:IOS_ASC_API_KEY_P8_BASE64; Id = $env:IOS_ASC_API_KEY_ID; Issuer = $env:IOS_ASC_API_ISSUER_ID }
    $env:IOS_ASC_API_KEY_P8_BASE64 = $null
    $env:IOS_ASC_API_KEY_ID = $null
    $env:IOS_ASC_API_ISSUER_ID = $null
    $number = $env:IOS_BUILD_NUMBER
    & pwsh -NoProfile -File (Join-Path $PSScriptRoot 'build.ps1') -Target device -Sign -BuildNumber $number |
        Tee-Object -Variable buildLines | Out-Host
    Assert-LastExit 'tools/ios/build.ps1 -Target device -Sign'
    $ipa = "$(@($buildLines)[-1])".Trim()
    if (-not $ipa.EndsWith('.ipa') -or -not (Test-Path -LiteralPath $ipa -PathType Leaf)) { throw "tools/ios/build.ps1 printed no .ipa path last (got '$ipa')" }
    if ($SkipUpload) {
        Write-Output "testflight: signed build $number at $ipa"
        return
    }
    Invoke-Upload $ipa $key
    Write-Output "testflight: uploaded build $number"
}

if ($OnMac) {
    if (-not $IsMacOS) { throw 'tools/ios/testflight.ps1 -OnMac runs on the Mac' }
    Invoke-OnMac -SkipUpload:$NoUpload
}
else {
    Invoke-FromPc -SkipUpload:$NoUpload
}
