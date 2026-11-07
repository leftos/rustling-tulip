#-------------------------------------------------------------------------------
# tools/sim/sim.ps1 -- drive the iPhone Simulator on the Mac mini builder.
#
# Booting, opening a URL and screenshots go through `xcrun simctl`; tapping, typing and the
# accessibility tree go through `idb` (Meta's iOS Development Bridge), which simctl lacks. Every
# verb is one or more ssh calls to builder@mac-mini, run through a login zsh so the login PATH
# applies. See tools/sim/README.md.
#
# The script declares no parameters on purpose, as tools/rt-mac.ps1 explains: PowerShell binds a
# script's parameters before its body runs, so `sim.ps1 tap -5 10` would bind -5 to a parameter
# rather than reach the verb. With no param() block every argument arrives in $args as typed.
#
#   pwsh -NoProfile -File tools/sim/sim.ps1 <verb> [<arguments>]
#
# SIM_PRINT_ONLY=1 prints the ssh and scp command lines it would run and exits 0, touching
# nothing. A device id the script would look up on the Mac prints as <udid>, and the remote
# command appears inside the ssh word with its own quoting intact.
#-------------------------------------------------------------------------------
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$MacBuilder = 'builder@mac-mini'
$DefaultDevice = 'iPhone 17'
$RemoteShot = '/tmp/sim-rt-shot.png'
$PrintOnly = ($env:SIM_PRINT_ONLY -eq '1')

# The client's path and companion. ~/.local/bin is not on the builder's login PATH, so the client
# is called by path, and IDB_COMPANION_PATH names the companion, which fb-idb reads before it
# looks for idb_companion on PATH.
$IdbPrefix = 'PATH="$HOME/.local/bin:$PATH"; IDB_COMPANION_PATH="$HOME/.local/bin/idb_companion"; '

# Installs idb into builder's home with no admin rights, one ssh call, and only what is missing.
# The companion archive is a flat tree whose idb_companion needs Resources/ beside it, so it is
# unpacked whole; a download that does not match its checksum installs nothing.
$SetupSnippet = 'if [ ! -x "$HOME/.local/opt/idb-companion-1.7.8/idb_companion" ]; then ' +
'curl -fsSL -o /tmp/idb-companion.tar.gz ' +
'https://github.com/facebook/idb/releases/download/v1.7.8/idb-companion.macos-arm64.tar.gz || exit 1; ' +
'if ! echo "44369a2df0b62e3a1764de61bd09280999c7e9b27ea8f20bff1ea3a9db0302a2  /tmp/idb-companion.tar.gz" ' +
'| shasum -a 256 -c -; then rm -f /tmp/idb-companion.tar.gz; ' +
'echo "the idb companion download does not match its checksum" >&2; exit 1; fi; ' +
'mkdir -p "$HOME/.local/opt/idb-companion-1.7.8" && ' +
'tar -xzf /tmp/idb-companion.tar.gz -C "$HOME/.local/opt/idb-companion-1.7.8" || exit 1; ' +
'rm -f /tmp/idb-companion.tar.gz; ' +
'[ -x "$HOME/.local/opt/idb-companion-1.7.8/idb_companion" ] || ' +
'{ echo "the idb companion did not unpack" >&2; exit 1; }; ' +
'mkdir -p "$HOME/.local/bin" && ' +
'ln -sfn "$HOME/.local/opt/idb-companion-1.7.8/idb_companion" "$HOME/.local/bin/idb_companion" ' +
'|| exit 1; fi; ' +
'if [ ! -x "$HOME/.local/bin/idb" ]; then ' +
'curl -fsSL -o /tmp/fb_idb-1.7.8-py3-none-any.whl ' +
'https://github.com/facebook/idb/releases/download/v1.7.8/fb_idb-1.7.8-py3-none-any.whl || exit 1; ' +
'if ! echo "ed8557a4c54ffee53006c1fe63f070cda567949312ab24be56faf987c5a3d04b' +
'  /tmp/fb_idb-1.7.8-py3-none-any.whl" | shasum -a 256 -c -; then ' +
'rm -f /tmp/fb_idb-1.7.8-py3-none-any.whl; ' +
'echo "the idb client download does not match its checksum" >&2; exit 1; fi; ' +
'/Users/builder/gate-tools/bin/uv tool install /tmp/fb_idb-1.7.8-py3-none-any.whl || ' +
'{ rm -f /tmp/fb_idb-1.7.8-py3-none-any.whl; exit 1; }; ' +
'rm -f /tmp/fb_idb-1.7.8-py3-none-any.whl; fi; ' +
'"$HOME/.local/bin/idb_companion" --version | head -3; ' +
'/Users/builder/gate-tools/bin/uv tool list | grep -F fb-idb'

$VerbList = @(
    'sim.ps1 setup                       install idb_companion and the idb client on the Mac mini',
    'sim.ps1 boot [<device name>]        boot a simulator (default iPhone 17) and wait until it is up',
    'sim.ps1 open <url>                  open a URL in Mobile Safari on the booted simulator',
    'sim.ps1 shot <local path.png>       screenshot the booted simulator to a local PNG',
    'sim.ps1 tap <x> <y>                 tap a point, in the points describe reports',
    'sim.ps1 swipe <x1> <y1> <x2> <y2>   swipe between two points',
    'sim.ps1 text <string>               type a string',
    'sim.ps1 key <keycode>               press a key',
    'sim.ps1 describe                    print the accessibility tree as JSON',
    'sim.ps1 shutdown                    shut the booted simulator down'
)

# Single-quotes a value for zsh, escaping an embedded quote the shell way.
function ConvertTo-ZshQuoted([string] $Value) {
    return "'" + $Value.Replace("'", "'\''") + "'"
}

function Write-Command([string] $Line) {
    [Console]::Out.WriteLine($Line)
}

function Exit-Sim([string] $Message) {
    [Console]::Error.WriteLine("sim: $Message")
    exit 1
}

function Exit-Usage([string] $Message) {
    [Console]::Error.WriteLine("sim: $Message")
    foreach ($line in $VerbList) { Write-Command $line }
    exit 2
}

# The last line a failed command wrote to stderr, or its exit code when it wrote none.
function Get-FailureLine([string] $Path, [int] $Code) {
    $lines = @(Get-Content -Path $Path -ErrorAction SilentlyContinue | Where-Object { $_.Trim() })
    if ($lines.Count -gt 0) { return $lines[-1].Trim() }
    return "exit code $Code"
}

# Runs one command on the Mac over ssh. Print-only mode prints the command line instead.
function Invoke-Remote {
    param([string] $Verb, [string] $Snippet)

    if ($PrintOnly) {
        Write-Command "ssh -o BatchMode=yes $MacBuilder zsh -lc '$Snippet'"
        return
    }

    $errorFile = Join-Path ([System.IO.Path]::GetTempPath()) ('sim-' + [guid]::NewGuid().ToString('N') + '.err')
    try {
        $output = & ssh -o BatchMode=yes $MacBuilder ('zsh -lc ' + (ConvertTo-ZshQuoted $Snippet)) 2> $errorFile
        $code = $LASTEXITCODE
        if ($code -ne 0) { Exit-Sim ("$Verb failed: " + (Get-FailureLine $errorFile $code)) }
        return $output
    }
    finally {
        Remove-Item -Path $errorFile -ErrorAction SilentlyContinue
    }
}

# Copies a file the Mac wrote back to this PC. Print-only mode prints the scp line instead.
function Copy-FromRemote {
    param([string] $Verb, [string] $RemotePath, [string] $LocalPath)

    if ($PrintOnly) {
        Write-Command ("scp -o BatchMode=yes {0}:{1} {2}" -f $MacBuilder, $RemotePath, (ConvertTo-ZshQuoted $LocalPath))
        return
    }

    $errorFile = Join-Path ([System.IO.Path]::GetTempPath()) ('sim-' + [guid]::NewGuid().ToString('N') + '.err')
    try {
        & scp -o BatchMode=yes "${MacBuilder}:$RemotePath" $LocalPath 2> $errorFile | Out-Null
        $code = $LASTEXITCODE
        if ($code -ne 0) { Exit-Sim ("$Verb failed: " + (Get-FailureLine $errorFile $code)) }
    }
    finally {
        Remove-Item -Path $errorFile -ErrorAction SilentlyContinue
    }
}

# The device line `xcrun simctl list devices` prints for the named simulator, or $null.
function Find-Device([string[]] $Listing, [string] $Name) {
    $pattern = '^\s+' + [regex]::Escape($Name) + ' \(([0-9A-Fa-f-]{36})\) \(([A-Za-z]+)\)'
    foreach ($line in $Listing) {
        if ($line -match $pattern) {
            return [pscustomobject]@{ Udid = $Matches[1]; State = $Matches[2] }
        }
    }
    return $null
}

# The booted simulator's id, printed as <udid> in print-only mode, which reads nothing on the Mac.
function Get-BootedUdid([string] $Verb) {
    $listing = @(Invoke-Remote -Verb $Verb -Snippet 'xcrun simctl list devices')
    if ($PrintOnly) { return '<udid>' }
    foreach ($line in $listing) {
        if ($line -match '\(([0-9A-Fa-f-]{36})\) \(Booted\)') { return $Matches[1] }
    }
    Exit-Sim 'no booted simulator; run sim.ps1 boot'
}

function Invoke-Boot([string] $Device) {
    $listing = @(Invoke-Remote -Verb 'boot' -Snippet 'xcrun simctl list devices')
    $match = $null
    if (-not $PrintOnly) {
        $match = Find-Device -Listing $listing -Name $Device
        if ($null -eq $match) { Exit-Sim "boot failed: no simulator named $Device" }
    }
    $udid = if ($null -eq $match) { '<udid>' } else { $match.Udid }
    if ($null -eq $match -or $match.State -ne 'Booted') {
        Invoke-Remote -Verb 'boot' -Snippet "xcrun simctl boot $udid" | Out-Null
    }
    Invoke-Remote -Verb 'boot' -Snippet "xcrun simctl bootstatus $udid -b >/dev/null" | Out-Null
    if (-not $PrintOnly) { Write-Output $udid }
}

function Invoke-Shot([string] $LocalPath) {
    Get-BootedUdid 'shot' | Out-Null
    $local = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LocalPath)
    Invoke-Remote -Verb 'shot' -Snippet "xcrun simctl io booted screenshot $RemoteShot" | Out-Null
    Copy-FromRemote -Verb 'shot' -RemotePath $RemoteShot -LocalPath $local
    Invoke-Remote -Verb 'shot' -Snippet "rm -f $RemoteShot" | Out-Null
    if (-not $PrintOnly) { Write-Output $local }
}

function Invoke-Idb {
    param([string] $Verb, [string] $Arguments, [switch] $ShowOutput)

    $udid = Get-BootedUdid $Verb
    $output = Invoke-Remote -Verb $Verb -Snippet ($IdbPrefix + '"$HOME/.local/bin/idb" ui ' + $Arguments + ' --udid ' + $udid)
    if ($ShowOutput.IsPresent) { Write-Output $output }
}

$verb = if ($args.Count -ge 1) { [string] $args[0] } else { '' }
$rest = @()
if ($args.Count -gt 1) { $rest = @($args[1..($args.Count - 1)]) }

switch ($verb) {
    'setup' {
        if ($rest.Count -ne 0) { Exit-Usage 'setup takes no arguments' }
        Invoke-Remote -Verb 'setup' -Snippet $SetupSnippet
    }
    'boot' {
        if ($rest.Count -gt 1) { Exit-Usage 'boot takes at most one device name' }
        $name = if ($rest.Count -eq 1) { [string] $rest[0] } else { $DefaultDevice }
        Invoke-Boot $name
    }
    'open' {
        if ($rest.Count -ne 1) { Exit-Usage 'open needs <url>' }
        Get-BootedUdid 'open' | Out-Null
        $snippet = 'xcrun simctl openurl booted ' + (ConvertTo-ZshQuoted ([string] $rest[0]))
        Invoke-Remote -Verb 'open' -Snippet $snippet | Out-Null
    }
    'shot' {
        if ($rest.Count -ne 1) { Exit-Usage 'shot needs <local path.png>' }
        Invoke-Shot ([string] $rest[0])
    }
    'tap' {
        if ($rest.Count -ne 2) { Exit-Usage 'tap needs <x> <y>' }
        Invoke-Idb -Verb 'tap' -Arguments ('tap {0} {1}' -f $rest[0], $rest[1])
    }
    'swipe' {
        if ($rest.Count -ne 4) { Exit-Usage 'swipe needs <x1> <y1> <x2> <y2>' }
        Invoke-Idb -Verb 'swipe' -Arguments ('swipe {0} {1} {2} {3}' -f $rest[0], $rest[1], $rest[2], $rest[3])
    }
    'text' {
        if ($rest.Count -ne 1) { Exit-Usage 'text needs <string>' }
        Invoke-Idb -Verb 'text' -Arguments ('text ' + (ConvertTo-ZshQuoted ([string] $rest[0])))
    }
    'key' {
        if ($rest.Count -ne 1) { Exit-Usage 'key needs <keycode>' }
        Invoke-Idb -Verb 'key' -Arguments ('key ' + [string] $rest[0])
    }
    'describe' {
        if ($rest.Count -ne 0) { Exit-Usage 'describe takes no arguments' }
        Invoke-Idb -Verb 'describe' -Arguments 'describe-all' -ShowOutput
    }
    'shutdown' {
        if ($rest.Count -ne 0) { Exit-Usage 'shutdown takes no arguments' }
        Get-BootedUdid 'shutdown' | Out-Null
        Invoke-Remote -Verb 'shutdown' -Snippet 'xcrun simctl shutdown booted' | Out-Null
    }
    default {
        Exit-Usage ("unknown verb '{0}'" -f $verb)
    }
}
