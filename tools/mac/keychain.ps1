<#
.SYNOPSIS
    The temporary signing keychain shared by tools/installer/pack-macos.ps1 and tools/ios/build.ps1,
    which dot-source this file: made, added to the user's keychain search list, and taken out again.

.DESCRIPTION
    A signing run adds its own keychain first in the user search list as it stands, dropping each
    entry whose keychain file no longer exists (one a killed run left behind), and at the end
    removes its own entry alone, so a keychain another run listed meanwhile stays listed. The
    cleanup only warns: a failure there never replaces the error that ended the run, and the next
    run drops an entry left dead.

    Get-KeychainListWithOurs and Get-KeychainListWithoutOurs work on plain values
    (tools/mac/keychain.tests.ps1); the other functions call `security` and run only on a Mac.
#>

# Every list is an array, even of one keychain: a lone string splatted into `security list-keychains -s`
# would pass each character as a keychain path and wreck the user's search list (yaat's pack-macos.ps1).

# Ours first, then each entry of Current whose keychain file exists, in its order; the rest are Pruned.
function Get-KeychainListWithOurs {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '', Justification = "'Ours' names our keychain, one entry")]
    param([string[]] $Current, [string] $Ours, [scriptblock] $Exists)
    $list = [Collections.Generic.List[string]]::new()
    $pruned = [Collections.Generic.List[string]]::new()
    $list.Add($Ours)
    foreach ($entry in $Current) {
        if ($entry -eq $Ours) { continue }
        if (& $Exists $entry) { $list.Add($entry) } else { $pruned.Add($entry) }
    }
    return @{ List = [string[]] $list.ToArray(); Pruned = [string[]] $pruned.ToArray() }
}

# Current less every entry equal to Ours, in its order.
function Get-KeychainListWithoutOurs {
    [Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseSingularNouns', '', Justification = "'Ours' names our keychain, one entry")]
    param([string[]] $Current, [string] $Ours)
    $kept = [Collections.Generic.List[string]]::new()
    foreach ($entry in $Current) {
        if ($entry -ne $Ours) { $kept.Add($entry) }
    }
    return , ([string[]] $kept.ToArray())
}

function Assert-SecurityExit([string] $What) {
    if ($LASTEXITCODE -ne 0) { throw "security $What failed (exit $LASTEXITCODE)" }
}

# The path with every symbolic link resolved (the Mac's temp folder sits under /var, a link to
# /private/var), so the keychain is listed, and found again in the list, under one spelling.
function Resolve-RealPath([string] $Path) {
    $real = @(& /bin/realpath $Path)
    if (($LASTEXITCODE -ne 0) -or ($real.Count -ne 1)) { throw "realpath $Path failed (exit $LASTEXITCODE)" }
    return "$($real[0])".Trim()
}

function Read-UserKeychainList {
    $lines = @(security list-keychains -d user)
    Assert-SecurityExit 'list-keychains -d user'
    $list = [string[]] @($lines | ForEach-Object { "$_".Trim().Trim('"') } | Where-Object { $_ })
    return , $list
}

function Write-UserKeychainList([string[]] $List) {
    security list-keychains -d user -s @List
    Assert-SecurityExit 'list-keychains -d user -s'
}

# A new keychain, unlocked, that locks itself again after an hour idle (the gate run's own limit).
function Initialize-SigningKeychain([string] $Keychain, [string] $Secret) {
    security create-keychain -p $Secret $Keychain
    Assert-SecurityExit 'create-keychain'
    security set-keychain-settings -lut 3600 $Keychain
    Assert-SecurityExit 'set-keychain-settings'
    security unlock-keychain -p $Secret $Keychain
    Assert-SecurityExit 'unlock-keychain'
}

# Ours first in the user search list, each entry whose keychain file is gone dropped with a warning.
function Add-SigningKeychain([string] $Keychain) {
    $current = Read-UserKeychainList
    # create-keychain has already listed ours, so an otherwise empty list holds only our entry.
    $others = @($current | Where-Object { $_ -ne $Keychain })
    if ($others.Count -eq 0) { throw 'unexpected user keychain search list (empty); fix it before signing' }
    $exists = { param($Path) Test-Path -LiteralPath $Path -PathType Leaf }
    $next = Get-KeychainListWithOurs -Current $current -Ours $Keychain -Exists $exists
    foreach ($gone in $next.Pruned) {
        Write-Warning "dropped $gone from the user keychain search list: its keychain file no longer exists"
    }
    Write-UserKeychainList $next.List
}

# Our entry out of the user search list, our keychain deleted and the work folder removed; each
# step tolerates what was never made, and a failure warns instead of throwing.
function Clear-SigningKeychain([string] $Keychain, [string] $WorkDir) {
    if ($Keychain) {
        try {
            $current = Read-UserKeychainList
            $kept = Get-KeychainListWithoutOurs -Current $current -Ours $Keychain
            if ($kept.Count -ne $current.Count) { Write-UserKeychainList $kept }
        }
        catch { Write-Warning "could not take $Keychain out of the user keychain search list: $_" }
        try {
            if (Test-Path -LiteralPath $Keychain) {
                security delete-keychain $Keychain
                Assert-SecurityExit 'delete-keychain'
            }
        }
        catch { Write-Warning "could not delete the keychain ${Keychain}: $_" }
    }
    try {
        if ($WorkDir -and (Test-Path -LiteralPath $WorkDir)) { Remove-Item -LiteralPath $WorkDir -Recurse -Force -ErrorAction Stop }
    }
    catch { Write-Warning "could not remove ${WorkDir}: $_" }
}
