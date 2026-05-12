<#
.SYNOPSIS
    PostToolUse hook: runs oxlint on edited TypeScript files and
    cargo clippy on edited Rust files.

.DESCRIPTION
    Reads Claude Code hook JSON from stdin, extracts the edited file path,
    and dispatches to the appropriate linter:

      - .ts / .tsx under apps/tauri-app/src/  -> oxlint <file>
      - .rs under crates/<name>/              -> cargo clippy -p <name>
      - .rs under apps/tauri-app/src-tauri/   -> cargo clippy -p rustling-tulip-app

    Cargo clippy is crate-scoped (the smallest unit it supports). Steady-state
    per-edit runs are ~2-3s for daemon, ~1.5s for protocol on a warm cache.

    Output is captured and surfaced to Claude via stderr (the convention for
    hook feedback). Files outside the watched paths are skipped silently.

    Exit codes:
      0  - No issues, or file skipped (silent success)
      1  - Lint issues found (non-fatal; Claude sees them and can fix)
      2  - Tooling failure (linter not found, JSON parse error, etc.)
#>

$ErrorActionPreference = 'Stop'

function Invoke-Oxlint {
    param([string]$FilePath)

    $tauriRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..\apps\tauri-app')).Path
    Push-Location $tauriRoot
    try {
        $relative = Resolve-Path -Relative -LiteralPath $FilePath
        $output = & npx --no-install oxlint $relative 2>&1
        $exit = $LASTEXITCODE

        if ($exit -ne 0) {
            [Console]::Error.WriteLine("[oxlint] issues in ${relative}:")
            $output | ForEach-Object { [Console]::Error.WriteLine($_) }
            return 1
        }
        return 0
    } finally {
        Pop-Location
    }
}

function Resolve-CargoCrate {
    param([string]$NormalizedPath)

    if ($NormalizedPath -match '/apps/tauri-app/src-tauri/') {
        return 'rustling-tulip-app'
    }
    if ($NormalizedPath -match '/crates/([^/]+)/') {
        return $Matches[1]
    }
    return $null
}

function Invoke-Clippy {
    param([string]$Crate)

    $repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
    Push-Location $repoRoot
    try {
        $output = & cargo clippy -p $Crate --all-targets -- -D warnings 2>&1
        $exit = $LASTEXITCODE

        if ($exit -ne 0) {
            [Console]::Error.WriteLine("[clippy] issues in crate '${Crate}':")
            $output | ForEach-Object { [Console]::Error.WriteLine($_) }
            return 1
        }
        return 0
    } finally {
        Pop-Location
    }
}

try {
    $payloadJson = [Console]::In.ReadToEnd()
    if ([string]::IsNullOrWhiteSpace($payloadJson)) { exit 0 }

    $payload = $payloadJson | ConvertFrom-Json
    $filePath = $payload.tool_input.file_path
    if ([string]::IsNullOrWhiteSpace($filePath)) { exit 0 }

    $ext = [System.IO.Path]::GetExtension($filePath).ToLowerInvariant()
    $normalized = $filePath -replace '\\', '/'

    if ($ext -in '.ts', '.tsx') {
        if ($normalized -notmatch '/apps/tauri-app/src/') { exit 0 }
        exit (Invoke-Oxlint -FilePath $filePath)
    }

    if ($ext -eq '.rs') {
        $crate = Resolve-CargoCrate -NormalizedPath $normalized
        if (-not $crate) { exit 0 }
        exit (Invoke-Clippy -Crate $crate)
    }

    exit 0
} catch {
    [Console]::Error.WriteLine("[post-edit hook] error: $($_.Exception.Message)")
    exit 2
}
