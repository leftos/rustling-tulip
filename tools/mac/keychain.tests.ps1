<#
.SYNOPSIS
    Checks the keychain search list that tools/mac/keychain.ps1 writes when a signing run adds its
    temporary keychain and when it takes it out again.

.DESCRIPTION
    Each case calls one of the two pure list functions with plain paths and a stand-in for the
    file check, so no case runs `security` or needs a Mac.

    Run it with: pwsh -NoProfile -Command "Invoke-Pester -Path tools/mac/keychain.tests.ps1 -Output Detailed"
#>
[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSUseDeclaredVarsMoreThanAssignments', '', Justification = 'Pester shares BeforeAll variables with each It')]
param()

BeforeAll {
    . (Join-Path $PSScriptRoot 'keychain.ps1')
    $ours = '/t/ours.keychain-db'
    $login = '/k/login.keychain-db'
    $yaat = '/y/yaat-temp.keychain-db'
    $dead = '/gone/old-temp.keychain-db'
    $exists = { param($Path) $Path -ne $dead }
}

Describe 'Get-KeychainListWithOurs' {
    It 'puts ours first before the login keychain and prunes nothing' {
        $r = Get-KeychainListWithOurs -Current @($login) -Ours $ours -Exists $exists
        $r.List | Should -Be @($ours, $login)
        $r.Pruned -is [array] | Should -BeTrue
        $r.Pruned.Count | Should -Be 0
    }

    It 'prunes an entry whose keychain file is gone' {
        $r = Get-KeychainListWithOurs -Current @($login, $dead) -Ours $ours -Exists $exists
        $r.List | Should -Be @($ours, $login)
        $r.Pruned -is [array] | Should -BeTrue
        $r.Pruned | Should -Be @($dead)
    }

    It 'returns an array of ours alone when the list is empty' {
        $r = Get-KeychainListWithOurs -Current @() -Ours $ours -Exists $exists
        $r.List -is [array] | Should -BeTrue
        $r.List.Count | Should -Be 1
        $r.List[0] | Should -Be $ours
    }

    It 'lists ours once, first, when the list already holds it' {
        $r = Get-KeychainListWithOurs -Current @($login, $ours) -Ours $ours -Exists $exists
        $r.List | Should -Be @($ours, $login)
    }

    It "keeps another live run's keychain in its place" {
        $r = Get-KeychainListWithOurs -Current @($login, $yaat) -Ours $ours -Exists $exists
        $r.List | Should -Be @($ours, $login, $yaat)
        $r.Pruned.Count | Should -Be 0
    }
}

Describe 'Get-KeychainListWithoutOurs' {
    It 'removes ours and keeps the rest in order' {
        $r = Get-KeychainListWithoutOurs -Current @($ours, $login, $yaat) -Ours $ours
        $r | Should -Be @($login, $yaat)
    }

    It 'leaves the list unchanged when ours is absent' {
        $r = Get-KeychainListWithoutOurs -Current @($login, $yaat) -Ours $ours
        $r | Should -Be @($login, $yaat)
    }

    It 'returns an array of one, or an empty array, when that is what is left' {
        $one = Get-KeychainListWithoutOurs -Current @($ours, $login) -Ours $ours
        $one -is [array] | Should -BeTrue
        $one.Count | Should -Be 1
        $one[0] | Should -Be $login
        $none = Get-KeychainListWithoutOurs -Current @($ours) -Ours $ours
        $none -is [array] | Should -BeTrue
        $none.Count | Should -Be 0
    }
}
