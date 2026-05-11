#Requires -Version 7.0
<#
.SYNOPSIS
    Generate a placeholder app icon and fan it out to all Tauri sizes.

.DESCRIPTION
    Draws a 512x512 'RT' monogram on the app's accent color into
    `apps/tauri-app/src-tauri/icons/source.png`, then runs `pnpm tauri icon`
    to regenerate every size/format (icon.ico, icon.icns, *.png).

    Re-run this any time `apps/tauri-app/src-tauri/icons/source.png` changes
    or you want to revert to the placeholder.

.EXAMPLE
    .\tools\make-app-icon.ps1
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Resolve-Path (Join-Path $ScriptDir '..')
$AppDir    = Join-Path $RepoRoot 'apps/tauri-app'
$IconsDir  = Join-Path $AppDir 'src-tauri/icons'
$SourcePng = Join-Path $IconsDir 'source.png'

Add-Type -AssemblyName System.Drawing

$size = 512
# Match var(--accent) from src/styles.css. Keeps the placeholder visually
# consistent with the rest of the UI.
$bg = [System.Drawing.Color]::FromArgb(78, 168, 255)
$fg = [System.Drawing.Color]::White

$bmp = New-Object System.Drawing.Bitmap($size, $size)
try {
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    try {
        $g.SmoothingMode     = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit
        $g.Clear($bg)

        $font = New-Object System.Drawing.Font('Segoe UI', 220, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
        try {
            $brush = New-Object System.Drawing.SolidBrush($fg)
            try {
                $fmt = New-Object System.Drawing.StringFormat
                $fmt.Alignment     = [System.Drawing.StringAlignment]::Center
                $fmt.LineAlignment = [System.Drawing.StringAlignment]::Center
                $rect = New-Object System.Drawing.RectangleF(0, 0, $size, $size)
                $g.DrawString('RT', $font, $brush, $rect, $fmt)
            } finally { $brush.Dispose() }
        } finally { $font.Dispose() }
    } finally { $g.Dispose() }

    $bmp.Save($SourcePng, [System.Drawing.Imaging.ImageFormat]::Png)
} finally { $bmp.Dispose() }

Write-Host "==> Wrote source PNG: $SourcePng" -ForegroundColor Cyan

Push-Location $AppDir
try {
    & pnpm tauri icon (Resolve-Path $SourcePng).Path
    if ($LASTEXITCODE -ne 0) { throw "tauri icon failed (exit $LASTEXITCODE)" }
}
finally { Pop-Location }

Write-Host "==> Icons regenerated under $IconsDir" -ForegroundColor Green
