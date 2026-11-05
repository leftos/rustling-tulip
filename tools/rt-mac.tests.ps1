<#
.SYNOPSIS
    Checks that tools/rt-mac.ps1 hands every cargo argument to the gate's cargo command unchanged.

.DESCRIPTION
    Each case runs the script as a child process with RT_MAC_PRINT_ONLY=1, which prints the gate
    command line it would run and exits, so nothing reaches the Mac mini. A case asserts on the end
    of that line: the words after the gate's `--` are the cargo command.

    Run it with: pwsh -NoProfile -Command "Invoke-Pester -Path tools/rt-mac.tests.ps1 -Output Detailed"
#>

BeforeAll {
    $repoRoot = Split-Path -Parent $PSScriptRoot
    $rtMac = Join-Path $repoRoot 'tools/rt-mac.ps1'
    $rt = Join-Path $repoRoot 'rt.ps1'
    $env:RT_MAC_PRINT_ONLY = '1'

    # Runs the script with the given words and returns its exit code and output.
    function Invoke-RtMac {
        param([string[]] $Words)
        $out = & pwsh -NoProfile -File $rtMac @Words 2>&1
        return [pscustomobject]@{
            ExitCode = $LASTEXITCODE
            Output   = ($out | Out-String).Trim()
        }
    }
}

AfterAll {
    $env:RT_MAC_PRINT_ONLY = $null
}

Describe 'tools/rt-mac.ps1' {
    It 'passes --package, --test and -- through to nextest' {
        $r = Invoke-RtMac @('test', '--package', 'daemon-client', '--test', 'smoke_macos', '--', '--ignored')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo nextest run --cargo-profile test-release --package daemon-client --test smoke_macos -- --ignored') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'passes -E and its filterset through unchanged' {
        $r = Invoke-RtMac @('test', '--package', 'rustling-tulip-native', '--lib', '-E', 'test(/^update::/)')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo nextest run --cargo-profile test-release --package rustling-tulip-native --lib -E test(/^update::/)') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'runs the workspace when test gets no extra argument' {
        $r = Invoke-RtMac @('test')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo nextest run --cargo-profile test-release --workspace') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'keeps --all-targets and the deny flag on clippy with extras' {
        $r = Invoke-RtMac @('clippy', '--package', 'daemon')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo clippy --package daemon --all-targets -- -D warnings') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'puts --all-targets before a bare -- and the deny flag after it on clippy' {
        $r = Invoke-RtMac @('clippy', '-p', 'daemon', '--', '-W', 'clippy::pedantic')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo clippy -p daemon --all-targets -- -W clippy::pedantic -D warnings') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'keeps --all-targets and the deny flag on a bare clippy' {
        $r = Invoke-RtMac @('clippy')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo clippy --workspace --all-targets -- -D warnings') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'rewrites --package to -p and keeps --all-targets on build' {
        $r = Invoke-RtMac @('build', '--package', 'daemon')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo build -p daemon --all-targets') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'runs the workspace when check gets no extra argument' {
        $r = Invoke-RtMac @('check')
        $r.ExitCode | Should -Be 0
        $r.Output.EndsWith('-- cargo check --workspace --all-targets') |
            Should -BeTrue -Because "printed [$($r.Output)]"
    }

    It 'refuses an unknown sub-verb' {
        $r = Invoke-RtMac @('frob')
        $r.ExitCode | Should -Not -Be 0
        $r.Output | Should -BeLike '*expected check, build, clippy or test*'
    }

    It 'prints the same line through rt.ps1 mac' {
        $out = & pwsh -NoProfile -File $rt mac build --package daemon 2>&1
        $code = $LASTEXITCODE
        $code | Should -Be 0
        ($out | Out-String).Trim().EndsWith('-- cargo build -p daemon --all-targets') |
            Should -BeTrue -Because "printed [$($out)]"
    }
}
