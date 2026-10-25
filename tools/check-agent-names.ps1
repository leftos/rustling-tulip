#!/usr/bin/env pwsh
# Seam check: agent names stay behind the plugin seam.
#
# Searches crates/, apps/ and tools/ for "claude", "anthropic" and "deepseek"
# (any case). Hits inside crates/agents/claude/ are the plugin's own; hits in
# test code are fixtures, not seams: a path with a tests/ or examples/ folder,
# the daemon's recorded screen fixtures, and in a .rs file each inline test
# module, from its `#[cfg(test)]` (or `#[cfg(all(test, ...))]`) attribute to
# the brace that closes its `mod name {`. Every other hit must match an entry of
# tools/agent-names-allow.txt, and every entry there must match a hit.
#
# Exit 0 prints "agent names ok: <n> allowed hits in <m> files"; exit 1 prints
# each violation as "path:line: text" and each unused entry as
# "unused allow entry: <entry>".

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Split-Path -Parent $PSScriptRoot
$allowFile = 'tools/agent-names-allow.txt'
$self = 'tools/check-agent-names.ps1'
$scanRoots = @('crates', 'apps', 'tools')

# A repo-relative glob as an anchored regex: `**/` is zero or more folders,
# `**` anything, `*` anything within one segment, `?` one character of one.
function ConvertTo-GlobRegex([string]$glob) {
    $out = [System.Text.StringBuilder]::new('^')
    $i = 0
    while ($i -lt $glob.Length) {
        $c = $glob[$i]
        if ($c -eq '*' -and $i + 1 -lt $glob.Length -and $glob[$i + 1] -eq '*') {
            if ($i + 2 -lt $glob.Length -and $glob[$i + 2] -eq '/') {
                [void]$out.Append('(?:.*/)?')
                $i += 3
            } else {
                [void]$out.Append('.*')
                $i += 2
            }
            continue
        }
        switch ($c) {
            '*' { [void]$out.Append('[^/]*') }
            '?' { [void]$out.Append('[^/]') }
            default { [void]$out.Append([regex]::Escape([string]$c)) }
        }
        $i++
    }
    [void]$out.Append('$')
    return [regex]::new($out.ToString())
}

function Read-AllowEntries([string]$path) {
    $entries = [System.Collections.Generic.List[object]]::new()
    $number = 0
    foreach ($raw in [System.IO.File]::ReadAllLines($path)) {
        $number++
        $line = $raw.Trim()
        if ($line -eq '' -or $line.StartsWith('#')) { continue }
        $m = [regex]::Match($line, '^(?<spec>\S.*?)\s+#\s+(?<reason>\S.*)$')
        if (-not $m.Success) {
            throw "${allowFile}:${number}: an entry needs a reason after ' # ': $line"
        }
        $spec = $m.Groups['spec'].Value
        $colon = $spec.IndexOf(':')
        $glob = if ($colon -lt 0) { $spec } else { $spec.Substring(0, $colon) }
        $pattern = if ($colon -lt 0) { $null } else { [regex]::new($spec.Substring($colon + 1)) }
        $entries.Add([pscustomobject]@{
                Spec    = $spec
                Path    = ConvertTo-GlobRegex $glob
                Pattern = $pattern
                Hits    = 0
            })
    }
    return $entries
}

# Matches of a search over the scan roots, as (path, line, text) with `/` paths.
function Invoke-Search([string[]]$rgArgs) {
    $output = & rg --no-heading --color never -n @rgArgs -- @scanRoots
    if ($LASTEXITCODE -gt 1) { throw "rg failed with exit code ${LASTEXITCODE}: rg $rgArgs" }
    foreach ($row in @($output | Where-Object { $_ })) {
        $m = [regex]::Match($row, '^(?<path>[^:]+):(?<line>\d+):(?<text>.*)$')
        if (-not $m.Success) { throw "unexpected rg output: $row" }
        [pscustomobject]@{
            Path = $m.Groups['path'].Value.Replace('\', '/')
            Line = [int]$m.Groups['line'].Value
            Text = $m.Groups['text'].Value
        }
    }
}

# The 0-based index of the `mod name {` line the test attribute on 1-based line
# `attribute` opens (after any further attributes), or $null when the attribute
# is on something else: a `mod name;` declaration, a `use` or a helper.
function Find-ModuleLine([string[]]$lines, [int]$attribute) {
    $inAttribute = $false
    for ($i = $attribute; $i -lt $lines.Length; $i++) {
        $text = $lines[$i].Trim()
        if ($inAttribute) {
            $inAttribute = -not $text.EndsWith(']')
            continue
        }
        if ($text -eq '' -or $text.StartsWith('//')) { continue }
        if ($text.StartsWith('#[')) {
            $inAttribute = -not $text.EndsWith(']')
            continue
        }
        if ($text -match '^(pub(\([\w:]+\))?\s+)?mod\s+\w+\s*\{') { return $i }
        return $null
    }
    return $null
}

# The index just past the end of the literal or block comment `state` is in,
# searching `text` from `from`, or -1 when it runs past the line.
function Find-LiteralEnd([string]$text, [int]$from, [hashtable]$state) {
    if ($state.Verbatim) {
        $at = $text.IndexOf($state.Close, $from)
        if ($at -lt 0) { return -1 }
        return $at + $state.Close.Length
    }
    for ($i = $from; $i -lt $text.Length; $i++) {
        if ($text[$i] -eq '\') { $i++; continue }
        if ($text[$i] -eq '"') { return $i + 1 }
    }
    return -1
}

# Opening minus closing braces on one Rust line, outside string, raw-string and
# char literals and comments. `state` carries a literal or block comment that
# runs past the line's end into the next call.
function Get-BraceDelta([string]$text, [hashtable]$state) {
    $delta = 0
    $i = 0
    while ($i -lt $text.Length) {
        if ($null -ne $state.Close) {
            $end = Find-LiteralEnd $text $i $state
            if ($end -lt 0) { return $delta }
            $state.Close = $null
            $i = $end
            continue
        }
        $rest = $text.Substring($i)
        if ($rest.StartsWith('//')) { return $delta }
        $opener = [regex]::Match($rest, '^(?:/\*|b?r(#*)"|b?")')
        if ($opener.Success) {
            $state.Verbatim = $opener.Value -ne '"' -and $opener.Value -ne 'b"'
            $state.Close = if ($opener.Value -eq '/*') { '*/' } else { '"' + $opener.Groups[1].Value }
            $i += $opener.Length
            continue
        }
        $char = [regex]::Match($rest, "^'(?:\\.[^']*|[^\\'])'")
        if ($char.Success) { $i += $char.Length; continue }
        if ($text[$i] -eq '{') { $delta++ } elseif ($text[$i] -eq '}') { $delta-- }
        $i++
    }
    return $delta
}

# The 1-based line of the brace that closes the block opened on 0-based line
# `from`, or the file's last line when it never closes.
function Find-ClosingLine([string[]]$lines, [int]$from) {
    $depth = 0
    $state = @{ Close = $null; Verbatim = $false }
    for ($n = $from; $n -lt $lines.Length; $n++) {
        $depth += Get-BraceDelta $lines[$n] $state
        if ($depth -le 0) { return $n + 1 }
    }
    return $lines.Length
}

# Each inline test module in a file as its 1-based (Start, End) lines: from its
# opening test attribute to the brace that closes it.
function Find-TestModuleSpans([string[]]$lines, [int[]]$attributeLines) {
    foreach ($attribute in $attributeLines) {
        $module = Find-ModuleLine $lines $attribute
        if ($null -eq $module) { continue }
        [pscustomobject]@{ Start = $attribute; End = (Find-ClosingLine $lines $module) }
    }
}

function Test-InTestModule([object[]]$spans, [int]$line) {
    foreach ($span in $spans) {
        if ($line -ge $span.Start -and $line -le $span.End) { return $true }
    }
    return $false
}

function Test-TestPath([string]$path) {
    if ($path.StartsWith('crates/agents/claude/')) { return $true }
    if ($path.StartsWith('crates/daemon/src/screen/fixtures/')) { return $true }
    if ($path -eq $self -or $path -eq $allowFile) { return $true }
    $folders = $path.Split('/')
    $folders = $folders[0..($folders.Length - 2)]
    return ($folders -contains 'tests') -or ($folders -contains 'examples')
}

Push-Location $root
try {
    $entries = Read-AllowEntries (Join-Path $root $allowFile)

    $testAttributes = @{}
    foreach ($hit in Invoke-Search @('-g', '*.rs', '^\s*#\[cfg\((all\()?test[,)]')) {
        if (-not $testAttributes.ContainsKey($hit.Path)) { $testAttributes[$hit.Path] = [System.Collections.Generic.List[int]]::new() }
        $testAttributes[$hit.Path].Add($hit.Line)
    }
    $testSpans = @{}
    foreach ($path in $testAttributes.Keys) {
        $lines = [System.IO.File]::ReadAllLines((Join-Path $root $path))
        $testSpans[$path] = @(Find-TestModuleSpans $lines $testAttributes[$path])
    }

    $violations = [System.Collections.Generic.List[string]]::new()
    $allowedFiles = [System.Collections.Generic.HashSet[string]]::new()
    $allowed = 0
    foreach ($hit in Invoke-Search @('-i', 'claude|anthropic|deepseek')) {
        if (Test-TestPath $hit.Path) { continue }
        if ($testSpans.ContainsKey($hit.Path) -and (Test-InTestModule $testSpans[$hit.Path] $hit.Line)) { continue }
        $matched = $false
        foreach ($entry in $entries) {
            if (-not $entry.Path.IsMatch($hit.Path)) { continue }
            if ($null -ne $entry.Pattern -and -not $entry.Pattern.IsMatch($hit.Text)) { continue }
            $entry.Hits++
            $matched = $true
        }
        if ($matched) {
            $allowed++
            [void]$allowedFiles.Add($hit.Path)
        } else {
            $violations.Add("$($hit.Path):$($hit.Line): $($hit.Text.Trim())")
        }
    }

    $unused = @($entries | Where-Object { $_.Hits -eq 0 })
    foreach ($violation in $violations) { Write-Output $violation }
    foreach ($entry in $unused) { Write-Output "unused allow entry: $($entry.Spec)" }
    if ($violations.Count -gt 0 -or $unused.Count -gt 0) {
        Write-Output "agent names outside the seam: $($violations.Count) hits not in $allowFile, $($unused.Count) unused entries"
        exit 1
    }
    Write-Output "agent names ok: $allowed allowed hits in $($allowedFiles.Count) files"
    exit 0
} finally {
    Pop-Location
}
