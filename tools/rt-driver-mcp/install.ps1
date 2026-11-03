#Requires -Version 7
<#
.SYNOPSIS
    Installs rt-driver-mcp: a release build, copied into a version folder of
    its own, so an install never touches a server that is running.
.DESCRIPTION
    Builds `cargo build -p rt-driver-mcp --release` through tools/gate.ps1,
    copies the exe to
    %LOCALAPPDATA%\leftos\rustling-tulip\data\driver-mcp\<version>-<sha256 prefix>\rt-driver-mcp.exe
    (skipped when that folder exists), and writes that folder's name to
    current.txt beside it, which tools/rt-driver-mcp/launch.ps1 reads.
#>
$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

function Fail([string]$message) {
    [Console]::Error.WriteLine("install.ps1: $message")
    exit 1
}

$sha = (& git -C $repo rev-parse --short=12 HEAD)
if ($LASTEXITCODE -ne 0 -or -not $sha) { Fail 'cannot read the checkout''s commit (git rev-parse)' }

& pwsh -NoProfile -File (Join-Path $repo 'tools/gate.ps1') run --cd $repo --log .tmp/rt-driver-mcp-install.log `
    --timeout 1800 --slot heavy --env "RT_DRIVER_MCP_SHA=$sha" -- cargo build -p rt-driver-mcp --release
if ($LASTEXITCODE -ne 0) { Fail "the release build failed (exit $LASTEXITCODE); see .tmp/rt-driver-mcp-install.log" }

$metadata = & cargo metadata --manifest-path (Join-Path $repo 'Cargo.toml') --format-version 1 --no-deps | ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { Fail 'cargo metadata failed' }
$package = $metadata.packages | Where-Object { $_.name -eq 'rt-driver-mcp' }
if (-not $package) { Fail 'cargo metadata lists no rt-driver-mcp package' }
$exe = Join-Path $metadata.target_directory 'release/rt-driver-mcp.exe'
if (-not (Test-Path -LiteralPath $exe)) { Fail "the build left no $exe" }

$hash = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant().Substring(0, 12)
$name = "$($package.version)-$hash"
$root = Join-Path $env:LOCALAPPDATA 'leftos\rustling-tulip\data\driver-mcp'
$folder = Join-Path $root $name
if (Test-Path -LiteralPath $folder) {
    [Console]::Error.WriteLine("install.ps1: $name is already installed")
} else {
    New-Item -ItemType Directory -Force -Path $folder | Out-Null
    Copy-Item -LiteralPath $exe -Destination (Join-Path $folder 'rt-driver-mcp.exe')
    [Console]::Error.WriteLine("install.ps1: installed $name ($sha) to $folder")
}
[System.IO.File]::WriteAllText((Join-Path $root 'current.txt'), "$name`n", [System.Text.UTF8Encoding]::new($false))
[Console]::Error.WriteLine("install.ps1: current.txt names $name; a client reconnecting to rt-driver picks it up")
