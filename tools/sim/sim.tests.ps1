<#
.SYNOPSIS
    Checks the ssh and scp command lines tools/sim/sim.ps1 builds for the Mac mini.

.DESCRIPTION
    Each case runs the script as a child process with SIM_PRINT_ONLY=1, which prints the command
    lines it would run and exits, so nothing reaches the Mac mini. A device the script has to look
    up on the Mac prints as <udid>: print-only mode touches the Mac not at all, so it cannot know
    the id.

    Run it with: pwsh -NoProfile -Command "Invoke-Pester -Path tools/sim/sim.tests.ps1 -Output Detailed"
#>

BeforeAll {
    $repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
    $sim = Join-Path $repoRoot 'tools/sim/sim.ps1'
    $env:SIM_PRINT_ONLY = '1'

    # Runs the script with the given words and returns its exit code and its lines.
    function Invoke-Sim {
        param([string[]] $Words)
        $out = & pwsh -NoProfile -File $sim @Words 2>&1
        return [pscustomobject]@{
            ExitCode = $LASTEXITCODE
            Lines    = @($out | ForEach-Object { [string] $_ })
            Output   = ($out | Out-String).Trim()
        }
    }
}

AfterAll {
    $env:SIM_PRINT_ONLY = $null
}

Describe 'tools/sim/sim.ps1' {
    It 'runs setup as one ssh command that installs into the home folder without admin rights' {
        $r = Invoke-Sim @('setup')
        $r.ExitCode | Should -Be 0
        $r.Lines.Count | Should -Be 1
        $r.Lines[0] | Should -BeLike 'ssh -o BatchMode=yes builder@mac-mini zsh -lc *'
        $r.Lines[0] | Should -BeLike '*download/v1.7.8/idb-companion.macos-arm64.tar.gz*'
        $r.Lines[0] | Should -BeLike '*44369a2df0b62e3a1764de61bd09280999c7e9b27ea8f20bff1ea3a9db0302a2*'
        $r.Lines[0] | Should -BeLike '*download/v1.7.8/fb_idb-1.7.8-py3-none-any.whl*'
        $r.Lines[0] | Should -BeLike '*ed8557a4c54ffee53006c1fe63f070cda567949312ab24be56faf987c5a3d04b*'
        $r.Lines[0] | Should -BeLike '*uv tool install /tmp/fb_idb-1.7.8-py3-none-any.whl*'
        $r.Lines[0] | Should -Not -BeLike '*brew*'
    }

    It 'boots the default device and waits for it, opening no window' {
        $r = Invoke-Sim @('boot')
        $r.ExitCode | Should -Be 0
        $r.Lines.Count | Should -Be 3
        $r.Lines[0] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl list devices'"
        $r.Lines[1] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl boot <udid>'"
        $r.Lines[2] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl bootstatus <udid> -b >/dev/null'"
    }

    It 'takes a device name on boot' {
        $r = Invoke-Sim @('boot', 'iPhone 18 Pro')
        $r.ExitCode | Should -Be 0
        $r.Lines.Count | Should -Be 3
        $r.Lines[2] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl bootstatus <udid> -b >/dev/null'"
    }

    It 'opens a URL in the booted simulator' {
        $r = Invoke-Sim @('open', 'https://example.com')
        $r.ExitCode | Should -Be 0
        $r.Lines[0] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl list devices'"
        $r.Lines[1].Contains("xcrun simctl openurl booted 'https://example.com'") | Should -BeTrue -Because "printed [$($r.Lines[1])]"
    }

    It 'screenshots, copies the png back and removes the remote file' {
        $local = Join-Path $repoRoot '.tmp/sim-test.png'
        $r = Invoke-Sim @('shot', $local)
        $r.ExitCode | Should -Be 0
        $r.Lines[0] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl list devices'"
        $r.Lines[1] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl io booted screenshot /tmp/sim-rt-shot.png'"
        $r.Lines[2] | Should -Be "scp -o BatchMode=yes builder@mac-mini:/tmp/sim-rt-shot.png '$local'"
        $r.Lines[3] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'rm -f /tmp/sim-rt-shot.png'"
    }

    It 'taps a point on the booted device with the client and companion in the home folder' {
        $r = Invoke-Sim @('tap', '200', '400')
        $r.ExitCode | Should -Be 0
        $r.Lines[1] | Should -BeLike '*IDB_COMPANION_PATH="$HOME/.local/bin/idb_companion"*'
        $r.Lines[1] | Should -BeLike '*"$HOME/.local/bin/idb" ui tap 200 400 --udid <udid>*'
    }

    It 'swipes between two points on the booted device' {
        $r = Invoke-Sim @('swipe', '100', '600', '100', '100')
        $r.ExitCode | Should -Be 0
        $r.Lines[1] | Should -BeLike '*ui swipe 100 600 100 100 --udid <udid>*'
    }

    It 'types text so zsh delivers spaces, quotes, $ and ; literally' {
        $r = Invoke-Sim @('text', 'it''s a $HOME; test')
        $r.ExitCode | Should -Be 0
        $expected = "ui text 'it'\''s a `$HOME; test' --udid <udid>"
        $r.Lines[1].Contains($expected) | Should -BeTrue -Because "printed [$($r.Lines[1])]"
    }

    It 'presses a key on the booted device' {
        $r = Invoke-Sim @('key', '40')
        $r.ExitCode | Should -Be 0
        $r.Lines[1] | Should -BeLike '*ui key 40 --udid <udid>*'
    }

    It 'prints the accessibility tree' {
        $r = Invoke-Sim @('describe')
        $r.ExitCode | Should -Be 0
        $r.Lines[1] | Should -BeLike '*ui describe-all --udid <udid>*'
    }

    It 'shuts the booted simulator down' {
        $r = Invoke-Sim @('shutdown')
        $r.ExitCode | Should -Be 0
        $r.Lines[1] | Should -Be "ssh -o BatchMode=yes builder@mac-mini zsh -lc 'xcrun simctl shutdown booted'"
    }

    It 'lists the verbs and exits 2 on an unknown verb' {
        $r = Invoke-Sim @('frobnicate')
        $r.ExitCode | Should -Be 2
        $r.Output | Should -BeLike '*sim.ps1 setup*'
        $r.Output | Should -BeLike '*sim.ps1 shutdown*'
    }

    It 'lists the verbs and exits 2 when a verb has too few arguments' {
        $r = Invoke-Sim @('tap', '200')
        $r.ExitCode | Should -Be 2
        $r.Output | Should -BeLike '*tap <x> <y>*'
    }
}
