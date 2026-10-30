<#
.SYNOPSIS
    Checks Get-PathWith and Get-PathWithout from user-path.ps1 without touching the registry.

.DESCRIPTION
    Prints one PASS or FAIL line per case and a final "<n> failed" line, and exits with the failure
    count. Run it with Windows PowerShell, which the installer uses:
    powershell.exe -NoProfile -ExecutionPolicy Bypass -File tools\installer\user-path.tests.ps1
#>
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'user-path.ps1')

$X = 'C:\Users\u\AppData\Local\Programs\rustling-tulip'
$XLower = 'c:\users\u\appdata\local\programs\rustling-tulip'

$entries = New-Object System.Collections.Generic.List[string]
$n = 0
while (([string]::Join(';', $entries.ToArray())).Length -lt 2500) {
    $entries.Add("C:\Tools\folder-number-$n\bin")
    $n++
}
$long = [string]::Join(';', $entries.ToArray())

$cases = @(
    @('With', '', $X, $X),
    @('With', 'a;b', $X, "a;b;$X"),
    @('With', 'a;b;', $X, "a;b;$X"),
    @('With', "a;$X;b", $X, "a;$X;b"),
    @('With', "a;$XLower;b", $X, "a;$XLower;b"),
    @('With', "a;$X\;b", $X, "a;$X\;b"),
    @('With', "a;$X", "$X\", "a;$X"),
    @('With', "a;${X}Y;b", $X, "a;${X}Y;b;$X"),
    @('With', '%USERPROFILE%\bin;b', $X, "%USERPROFILE%\bin;b;$X"),
    @('With', $long, $X, "$long;$X"),
    @('With', "$long;$X;$long", $X, "$long;$X;$long"),
    @('Without', "a;$X;b", $X, 'a;b'),
    @('Without', "$X;a", $X, 'a'),
    @('Without', "a;$X", $X, 'a'),
    @('Without', $X, $X, ''),
    @('Without', "$X;a;$XLower\;b;$X", $X, 'a;b'),
    @('Without', "a;$X;b", "$X\", 'a;b'),
    @('Without', "a;${X}Y;b", $X, "a;${X}Y;b"),
    @('Without', 'a;;b', $X, 'a;;b'),
    @('Without', '', $X, ''),
    @('Without', "$long;$X;%USERPROFILE%\bin", $X, "$long;%USERPROFILE%\bin")
)

$failures = 0
foreach ($case in $cases) {
    $fn, $list, $folder, $expected = $case
    $actual = if ($fn -eq 'With') { Get-PathWith -List $list -Folder $folder } else { Get-PathWithout -List $list -Folder $folder }
    $label = "$fn [$($list.Length) chars: $($list.Substring(0, [Math]::Min(60, $list.Length)))] [$folder]"
    if ($actual -ceq $expected) {
        Write-Output "PASS $label"
    } else {
        Write-Output "FAIL $label -> [$actual], expected [$expected]"
        $failures++
    }
}
# The -Dir argument as rustling-tulip.nsi's RT_USER_PATH macro quotes it: a trailing `\` is doubled so it
# never forms `\"` with the closing quote. Keep this in step with the macro.
function Get-NsisDirArgument {
    param([string] $Folder)
    if ($Folder.EndsWith('\')) {
        return '"' + $Folder + '\"'
    }
    return '"' + $Folder + '"'
}

# Runs the installer's command line against a stub user-path.ps1 that only echoes -Dir, through the
# 32-bit Windows PowerShell the 32-bit installer starts, and checks the folder arrives unchanged.
$stubDir = Join-Path ([IO.Path]::GetTempPath()) "rt-user-path-args-$PID"
[void] [IO.Directory]::CreateDirectory($stubDir)
$stub = Join-Path $stubDir 'user-path.ps1'
$stubText = @'
param([switch] $Add, [string] $Dir)
Write-Output "dir=[$Dir]"
'@
[IO.File]::WriteAllText($stub, $stubText)
$powershell = Join-Path $env:SystemRoot 'SysWOW64\WindowsPowerShell\v1.0\powershell.exe'
if (-not (Test-Path $powershell)) {
    $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
}
try {
    foreach ($folder in @('D:\', 'C:\Program Files\x\', 'C:\a''b$c\', $X)) {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $powershell
        $psi.Arguments = "-NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$stub`" -Add -Dir $(Get-NsisDirArgument -Folder $folder)"
        $psi.UseShellExecute = $false
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $process = [System.Diagnostics.Process]::Start($psi)
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $out = $process.StandardOutput.ReadToEnd().Trim()
        $process.WaitForExit()
        $err = $stderrTask.Result.Trim()
        $label = "-Dir argument [$folder]"
        if ($out -ceq "dir=[$folder]") {
            Write-Output "PASS $label"
        } else {
            Write-Output "FAIL $label -> [$out] stderr [$err]"
            $failures++
        }
    }
} finally {
    Remove-Item -LiteralPath $stub
    Remove-Item -LiteralPath $stubDir
}

Write-Output "$failures failed"
exit $failures
