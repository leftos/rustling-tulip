<#
.SYNOPSIS
    Adds a folder to, or removes it from, the current user's PATH (HKCU\Environment, value Path).

.DESCRIPTION
    The installer runs this with -Add and the uninstaller with -Remove, because an SSH client runs
    `rustling-tulipd bridge` on this host by name and Windows sshd starts it with the user's PATH
    from the registry.

    Entries compare case-insensitively, ignoring one trailing `\`. Every other entry keeps its text
    and order, `%VAR%` entries stay unexpanded, and the value keeps its kind (ExpandString when it
    was ExpandString or missing, String when it was String). A value left empty is deleted. After a
    change it broadcasts WM_SETTINGCHANGE ("Environment") so programs started next see it.

    Prints one line and exits 0 when the folder was added, removed, already present or already
    absent; prints the error and exits 1 when the PATH could not be read or written.

    Dot-sourced (as user-path.tests.ps1 does), it only defines Get-PathWith and Get-PathWithout.
#>
[CmdletBinding(DefaultParameterSetName = 'Library')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Add')]
    [switch] $Add,

    [Parameter(Mandatory, ParameterSetName = 'Remove')]
    [switch] $Remove,

    [Parameter(Mandatory, ParameterSetName = 'Add')]
    [Parameter(Mandatory, ParameterSetName = 'Remove')]
    [string] $Dir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Test-SamePathEntry {
    param([string] $Entry, [string] $Folder)
    $a = if ($Entry.EndsWith('\')) { $Entry.Substring(0, $Entry.Length - 1) } else { $Entry }
    $b = if ($Folder.EndsWith('\')) { $Folder.Substring(0, $Folder.Length - 1) } else { $Folder }
    return [string]::Equals($a, $b, [System.StringComparison]::OrdinalIgnoreCase)
}

<#
.SYNOPSIS
    Returns List with Folder appended as its last entry, or List unchanged when Folder is already one
    of its entries. An empty list becomes Folder alone; a list ending in `;` gets no second `;`.
#>
function Get-PathWith {
    param([string] $List, [string] $Folder)
    foreach ($entry in $List.Split(';')) {
        if (Test-SamePathEntry -Entry $entry -Folder $Folder) {
            return $List
        }
    }
    if ($List -eq '') {
        return $Folder
    }
    if ($List.EndsWith(';')) {
        return $List + $Folder
    }
    return $List + ';' + $Folder
}

<#
.SYNOPSIS
    Returns List with every entry naming Folder removed. Every other entry, empty ones included, keeps
    its text and order, so a list without Folder comes back unchanged and a removal leaves no `;` of
    its own behind.
#>
function Get-PathWithout {
    param([string] $List, [string] $Folder)
    $kept = New-Object System.Collections.Generic.List[string]
    foreach ($entry in $List.Split(';')) {
        if (-not (Test-SamePathEntry -Entry $entry -Folder $Folder)) {
            $kept.Add($entry)
        }
    }
    return [string]::Join(';', $kept.ToArray())
}

function Send-EnvironmentChange {
    Add-Type -Namespace RustlingTulip -Name UserPathNative -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(
    IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
    $hwndBroadcast = [IntPtr] 0xffff
    $wmSettingChange = 0x001A
    $smtoAbortIfHung = 0x0002
    $result = [UIntPtr]::Zero
    $sent = [RustlingTulip.UserPathNative]::SendMessageTimeout(
        $hwndBroadcast, $wmSettingChange, [UIntPtr]::Zero, 'Environment', $smtoAbortIfHung, 5000, [ref] $result)
    return $sent -ne [IntPtr]::Zero
}

# The PATH is already written when this runs, so a failed broadcast is only a note and never a failed update.
function Send-EnvironmentChangeOrNote {
    $note = 'The environment-change broadcast did not complete; programs started from Explorer may need a new sign-in to see the PATH.'
    try {
        if (-not (Send-EnvironmentChange)) {
            Write-Output $note
        }
    } catch {
        Write-Output "$note ($($_.Exception.Message))"
    }
}

function Update-UserPath {
    param([bool] $AddFolder, [string] $Folder)
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try {
        $exists = $key.GetValueNames() -contains 'Path'
        $kind = [Microsoft.Win32.RegistryValueKind]::ExpandString
        $old = ''
        if ($exists) {
            $kind = $key.GetValueKind('Path')
            if ($kind -ne [Microsoft.Win32.RegistryValueKind]::String -and $kind -ne [Microsoft.Win32.RegistryValueKind]::ExpandString) {
                throw "HKCU\Environment\Path is a $kind value, not a string; leaving it as it is."
            }
            $old = [string] $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        }

        $new = if ($AddFolder) { Get-PathWith -List $old -Folder $Folder } else { Get-PathWithout -List $old -Folder $Folder }
        if ($new -ceq $old) {
            if ($AddFolder) { Write-Output "$Folder is already on the user PATH." } else { Write-Output "$Folder is not on the user PATH." }
            return
        }

        if ($new -eq '') {
            $key.DeleteValue('Path')
        } else {
            $key.SetValue('Path', $new, $kind)
        }
        if ($AddFolder) { Write-Output "Added $Folder to the user PATH." } else { Write-Output "Removed $Folder from the user PATH." }
    } finally {
        $key.Dispose()
    }
    Send-EnvironmentChangeOrNote
}

if ($MyInvocation.InvocationName -eq '.') {
    return
}
if ($PSCmdlet.ParameterSetName -eq 'Library') {
    Write-Output 'user-path.ps1: pass -Add or -Remove with -Dir <folder>.'
    exit 2
}

try {
    Update-UserPath -AddFolder $Add.IsPresent -Folder $Dir
    exit 0
} catch {
    Write-Output "Could not update the user PATH: $($_.Exception.Message)"
    exit 1
}
