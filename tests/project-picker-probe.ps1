[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 2147483647)][int]$AppProcessId,
    [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
    [Parameter(Mandatory = $true)][ValidateLength(1, 200)][string]$ExpectedDialogText,
    [Parameter(Mandatory = $true)][ValidateSet('Select', 'Cancel')][string]$Mode,
    [string]$ExpectedPath
)

$ErrorActionPreference = 'Stop'
if (-not [IO.Path]::IsPathRooted($ExpectedExecutable)) { throw 'Expected executable must be an absolute file' }
$expectedExecutablePath = [IO.Path]::GetFullPath($ExpectedExecutable)
if (-not (Test-Path -LiteralPath $expectedExecutablePath -PathType Leaf) -or [IO.Path]::GetFileName($expectedExecutablePath) -ne 'agent-grid.exe') {
    throw 'Expected executable must be an existing agent-grid.exe'
}
if ($Mode -eq 'Select') {
    if (-not $ExpectedPath -or -not [IO.Path]::IsPathRooted($ExpectedPath) -or -not (Test-Path -LiteralPath $ExpectedPath -PathType Container)) {
        throw 'Select mode requires an existing absolute directory'
    }
    $ExpectedPath = [IO.Path]::GetFullPath($ExpectedPath)
}
$appProcess = Get-Process -Id $AppProcessId -ErrorAction Stop
$appProcess.Refresh()
$startTicks = $appProcess.StartTime.ToUniversalTime().Ticks
$currentSession = (Get-Process -Id $PID).SessionId
if ($appProcess.StartTime.ToUniversalTime() -lt [DateTime]::UtcNow.AddMinutes(-20)) { throw 'Refusing a process older than twenty minutes' }
if ($appProcess.SessionId -ne $currentSession) { throw 'Target is in another Windows session' }
if (-not [string]::Equals([IO.Path]::GetFullPath($appProcess.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target executable path does not match' }
$mainWindow = $appProcess.MainWindowHandle
if ($mainWindow -eq [IntPtr]::Zero -or $appProcess.MainWindowTitle -ne 'Agent Grid') { throw 'Target does not have the expected Agent Grid main window' }

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class AgentGridPickerProbe {
    private delegate bool EnumWindowProc(IntPtr hwnd, IntPtr data);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindowProc callback, IntPtr data);
    [DllImport("user32.dll")] private static extern bool EnumChildWindows(IntPtr hwnd, EnumWindowProc callback, IntPtr data);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassNameW(IntPtr hwnd, StringBuilder value, int length);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowEnabled(IntPtr hwnd);
    [DllImport("user32.dll")] private static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr hwnd, int id);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr hwnd);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr ReadMessage(IntPtr hwnd, uint message, UIntPtr wParam, StringBuilder text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr WriteMessage(IntPtr hwnd, uint message, UIntPtr wParam, string text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", ExactSpelling=true, SetLastError=true)]
    private static extern IntPtr PlainMessage(IntPtr hwnd, uint message, UIntPtr wParam, IntPtr data, uint flags, uint timeout, out UIntPtr result);
    public static string WindowClass(IntPtr hwnd) {
        var value = new StringBuilder(256);
        GetClassNameW(hwnd, value, value.Capacity);
        return value.ToString();
    }
    private static string ControlText(IntPtr hwnd) {
        var value = new StringBuilder(1024);
        UIntPtr ignored;
        if (ReadMessage(hwnd, 0x000D, new UIntPtr((uint)value.Capacity), value, 3, 1000, out ignored) == IntPtr.Zero) return null;
        return value.ToString();
    }
    public static bool OwnedBy(IntPtr hwnd, uint pid) {
        uint observed;
        GetWindowThreadProcessId(hwnd, out observed);
        return observed == pid;
    }
    public static bool IsExpectedDialog(IntPtr hwnd, uint pid, IntPtr owner, string body) {
        if (!IsWindow(hwnd) || !IsWindowVisible(hwnd) || !OwnedBy(hwnd, pid) || WindowClass(hwnd) != "#32770" || GetWindow(hwnd, 4) != owner) return false;
        bool found = false;
        int visited = 0;
        EnumChildWindows(hwnd, delegate(IntPtr child, IntPtr unused) {
            if (++visited > 128) return false;
            if (OwnedBy(child, pid) && WindowClass(child) == "Static" && ControlText(child) == body) { found = true; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }
    public static IntPtr[] FindDialogs(uint pid, IntPtr owner, string body) {
        var found = new List<IntPtr>();
        EnumWindows(delegate(IntPtr hwnd, IntPtr unused) {
            if (OwnedBy(hwnd, pid) && IsExpectedDialog(hwnd, pid, owner, body)) found.Add(hwnd);
            return true;
        }, IntPtr.Zero);
        return found.ToArray();
    }
    public static IntPtr[] Edits(IntPtr dialog, uint pid) {
        var found = new List<IntPtr>();
        EnumChildWindows(dialog, delegate(IntPtr child, IntPtr unused) {
            if (OwnedBy(child, pid) && IsWindowVisible(child) && WindowClass(child) == "Edit") found.Add(child);
            return true;
        }, IntPtr.Zero);
        return found.ToArray();
    }
    public static void SetEdit(IntPtr edit, string text) {
        UIntPtr accepted;
        if (WriteMessage(edit, 0x000C, UIntPtr.Zero, text, 3, 2000, out accepted) == IntPtr.Zero || accepted == UIntPtr.Zero)
            throw new InvalidOperationException("Owned path edit did not accept WM_SETTEXT");
    }
    public static void Click(IntPtr button) {
        UIntPtr ignored;
        if (PlainMessage(button, 0x00F5, UIntPtr.Zero, IntPtr.Zero, 3, 2000, out ignored) == IntPtr.Zero)
            throw new InvalidOperationException("Owned dialog button did not respond to BM_CLICK");
    }
}
'@

function Assert-ProcessIdentity {
    $observed = Get-Process -Id $AppProcessId -ErrorAction Stop
    $observed.Refresh()
    if ($observed.HasExited -or $observed.StartTime.ToUniversalTime().Ticks -ne $startTicks) { throw 'Target process exited or PID was reused' }
    if ($observed.SessionId -ne $currentSession -or -not [string]::Equals([IO.Path]::GetFullPath($observed.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target process identity changed' }
    if (-not [AgentGridPickerProbe]::IsWindow($mainWindow) -or -not [AgentGridPickerProbe]::OwnedBy($mainWindow, $AppProcessId)) { throw 'Original main window identity changed' }
}
function Assert-DialogIdentity {
    Assert-ProcessIdentity
    if (-not [AgentGridPickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogText)) { throw 'Original owned folder dialog identity changed' }
}
function Wait-DialogClosed([int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        Assert-ProcessIdentity
        if (-not [AgentGridPickerProbe]::IsWindow($script:dialog) -or -not [AgentGridPickerProbe]::IsWindowVisible($script:dialog)) { return }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Owned folder dialog did not close within the bounded wait'
}

$deadline = [DateTime]::UtcNow.AddSeconds(7)
$script:dialog = [IntPtr]::Zero
do {
    Assert-ProcessIdentity
    $matches = [AgentGridPickerProbe]::FindDialogs($AppProcessId, $mainWindow, $ExpectedDialogText)
    if ($matches.Length -gt 1) { throw 'Multiple matching owned folder dialogs; refusing ambiguous action' }
    if ($matches.Length -eq 1) { $script:dialog = $matches[0]; break }
    Start-Sleep -Milliseconds 50
} while ([DateTime]::UtcNow -lt $deadline)
if ($script:dialog -eq [IntPtr]::Zero) { throw 'Expected owned folder dialog was not found; no UI action was sent' }

$actions = New-Object 'System.Collections.Generic.List[string]'
$editId = $null
$primaryFailure = $null
$cleanupFailure = $null
try {
    Assert-DialogIdentity
    if ($Mode -eq 'Select') {
        $edits = [AgentGridPickerProbe]::Edits($script:dialog, $AppProcessId)
        if ($edits.Length -ne 1) { throw 'Expected one visible owned folder path edit; refusing ambiguous action' }
        $editId = [AgentGridPickerProbe]::GetDlgCtrlID($edits[0])
        [AgentGridPickerProbe]::SetEdit($edits[0], $ExpectedPath)
        $actions.Add('owned-edit-WM_SETTEXT')
        Assert-DialogIdentity
        $button = [AgentGridPickerProbe]::GetDlgItem($script:dialog, 1)
        if ($button -eq [IntPtr]::Zero -or -not [AgentGridPickerProbe]::OwnedBy($button, $AppProcessId) -or -not [AgentGridPickerProbe]::IsWindowEnabled($button)) { throw 'Owned IDOK button is missing or disabled' }
        [AgentGridPickerProbe]::Click($button)
        $actions.Add('owned-IDOK-BM_CLICK')
    } else {
        $button = [AgentGridPickerProbe]::GetDlgItem($script:dialog, 2)
        if ($button -eq [IntPtr]::Zero -or -not [AgentGridPickerProbe]::OwnedBy($button, $AppProcessId)) { throw 'Owned IDCANCEL button is missing' }
        [AgentGridPickerProbe]::Click($button)
        $actions.Add('owned-IDCANCEL-BM_CLICK')
    }
    Wait-DialogClosed 5
} catch {
    $primaryFailure = $_.Exception.Message
    try {
        Assert-ProcessIdentity
        if ([AgentGridPickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogText)) {
            $cancel = [AgentGridPickerProbe]::GetDlgItem($script:dialog, 2)
            if ($cancel -eq [IntPtr]::Zero -or -not [AgentGridPickerProbe]::OwnedBy($cancel, $AppProcessId)) { throw 'Cleanup refused an unverified cancel control' }
            [AgentGridPickerProbe]::Click($cancel)
            $actions.Add('failure-cleanup-owned-IDCANCEL')
            Wait-DialogClosed 2
        }
    } catch { $cleanupFailure = $_.Exception.Message }
}
if ($primaryFailure) {
    if ($cleanupFailure) { throw "Picker probe failed: $primaryFailure; cleanup failed: $cleanupFailure" }
    throw "Picker probe failed: $primaryFailure; original owned dialog is closed or no longer matches"
}
Assert-ProcessIdentity
[ordered]@{
    schema = 'agent-grid-project-picker-probe-v1'
    mode = $Mode
    processId = $AppProcessId
    processStartedAt = $appProcess.StartTime.ToUniversalTime().ToString('o')
    dialogHandle = $script:dialog.ToInt64().ToString()
    parentHandle = $mainWindow.ToInt64().ToString()
    exactProcessAndDialogVerified = $true
    dialogClosed = $true
    editControlId = $editId
    actions = @($actions.ToArray())
    selectionResultVerified = $false
    selectionResultOracle = 'Caller must verify returned project cwd and persisted registry; dialog closure alone is not selection proof'
    externalProcessesTouched = $false
} | ConvertTo-Json -Depth 4 -Compress
