[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 2147483647)][int]$AppProcessId,
    [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
    [Parameter(Mandatory = $true)][ValidateLength(1, 200)][string]$ExpectedDialogText,
    [Parameter(Mandatory = $true)][ValidateSet('Select', 'Cancel')][string]$Mode,
    [string]$ExpectedPath
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding=$OutputEncoding=[Text.UTF8Encoding]::new($false)
if (-not [IO.Path]::IsPathRooted($ExpectedExecutable)) { throw 'Expected executable must be absolute' }
$expectedExecutablePath = [IO.Path]::GetFullPath($ExpectedExecutable)
if (-not (Test-Path -LiteralPath $expectedExecutablePath -PathType Leaf) -or [IO.Path]::GetFileName($expectedExecutablePath) -ne 'opendiy-desktop.exe') { throw 'Expected executable must be an existing opendiy-desktop.exe' }
if ($Mode -eq 'Select') {
    if (-not $ExpectedPath -or -not [IO.Path]::IsPathRooted($ExpectedPath) -or -not (Test-Path -LiteralPath $ExpectedPath -PathType Container)) { throw 'Select mode requires an existing absolute directory' }
    $ExpectedPath = [IO.Path]::GetFullPath($ExpectedPath)
}
$appProcess = Get-Process -Id $AppProcessId -ErrorAction Stop
$appProcess.Refresh()
$startTicks = $appProcess.StartTime.ToUniversalTime().Ticks
$currentSession = (Get-Process -Id $PID).SessionId
if ($appProcess.StartTime.ToUniversalTime() -lt [DateTime]::UtcNow.AddMinutes(-20)) { throw 'Refusing a process older than twenty minutes' }
if ($appProcess.SessionId -ne $currentSession) { throw 'Target is in another Windows session' }
if (-not [string]::Equals([IO.Path]::GetFullPath($appProcess.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target executable path does not match' }
# The active native modal can be returned as Process.MainWindowHandle.
# Resolve the unique owned top-level app window by its exact title instead.

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
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetWindowTextW(IntPtr hwnd, StringBuilder value, int length);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
    [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")] private static extern bool IsWindowEnabled(IntPtr hwnd);
    [DllImport("user32.dll")] private static extern IntPtr GetWindow(IntPtr hwnd, uint command);
    [DllImport("user32.dll")] private static extern IntPtr GetParent(IntPtr hwnd);
    [DllImport("user32.dll")] private static extern IntPtr GetDlgItem(IntPtr hwnd, int id);
    [DllImport("user32.dll")] public static extern int GetDlgCtrlID(IntPtr hwnd);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr ReadMessage(IntPtr hwnd, uint message, UIntPtr wParam, StringBuilder text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", CharSet=CharSet.Unicode, SetLastError=true)]
    private static extern IntPtr WriteMessage(IntPtr hwnd, uint message, UIntPtr wParam, string text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", ExactSpelling=true, SetLastError=true)]
    private static extern IntPtr PlainMessage(IntPtr hwnd, uint message, UIntPtr wParam, IntPtr data, uint flags, uint timeout, out UIntPtr result);
    private static string WindowClass(IntPtr hwnd) { var text=new StringBuilder(256); GetClassNameW(hwnd,text,text.Capacity); return text.ToString(); }
    public static bool OwnedBy(IntPtr hwnd, uint pid) { uint observed; GetWindowThreadProcessId(hwnd,out observed); return observed==pid; }
    public static bool HasModernFolderView(IntPtr dialog, uint pid) {
        bool found=false; int visited=0;
        EnumChildWindows(dialog,delegate(IntPtr child,IntPtr unused) {
            if(++visited>512) return false;
            if(OwnedBy(child,pid) && IsWindowVisible(child) && WindowClass(child)=="DirectUIHWND") { found=true; return false; }
            return true;
        },IntPtr.Zero);
        return found;
    }
    public static bool IsExpectedDialog(IntPtr hwnd, uint pid, IntPtr owner, string title) {
        if (!IsWindow(hwnd) || !IsWindowVisible(hwnd) || !OwnedBy(hwnd,pid) || WindowClass(hwnd)!="#32770" || GetWindow(hwnd,4)!=owner) return false;
        var text=new StringBuilder(256); GetWindowTextW(hwnd,text,text.Capacity); return text.ToString()==title && HasModernFolderView(hwnd,pid);
    }
    public static IntPtr[] FindMainWindows(uint pid) {
        var matches=new List<IntPtr>();
        EnumWindows(delegate(IntPtr hwnd,IntPtr unused) {
            if(!OwnedBy(hwnd,pid) || !IsWindowVisible(hwnd) || GetWindow(hwnd,4)!=IntPtr.Zero || WindowClass(hwnd)=="#32770") return true;
            var text=new StringBuilder(256); GetWindowTextW(hwnd,text,text.Capacity);
            if(text.ToString()=="OpenDIY Desktop") matches.Add(hwnd);
            return true;
        },IntPtr.Zero);
        return matches.ToArray();
    }
    public static IntPtr[] FindDialogs(uint pid, IntPtr owner, string title) {
        var matches=new List<IntPtr>();
        EnumWindows(delegate(IntPtr hwnd,IntPtr unused) { if(OwnedBy(hwnd,pid) && IsExpectedDialog(hwnd,pid,owner,title)) matches.Add(hwnd); return true; },IntPtr.Zero);
        return matches.ToArray();
    }
    public static IntPtr[] FolderEdits(IntPtr dialog, uint pid) {
        var matches=new List<IntPtr>(); int visited=0;
        EnumChildWindows(dialog,delegate(IntPtr child,IntPtr unused) {
            if(++visited>512) return false;
            if(!OwnedBy(child,pid) || !IsWindowVisible(child) || !IsWindowEnabled(child) || WindowClass(child)!="Edit") return true;
            // The observed Windows folder picker uses edt1=1152 directly under
            // the owned modern dialog, paired with stc3=1090 "Folder:".
            // DirectUI plus the exact folder label distinguishes it from a classic dialog.
            var label=GetDlgItem(dialog,1090); var labelText=new StringBuilder(128);
            if(label!=IntPtr.Zero) GetWindowTextW(label,labelText,labelText.Capacity);
            bool filename=GetDlgCtrlID(child)==1152 && GetParent(child)==dialog
                && HasModernFolderView(dialog,pid) && label!=IntPtr.Zero && OwnedBy(label,pid)
                && IsWindowVisible(label) && WindowClass(label)=="Static"
                && (labelText.ToString()=="\uD3F4\uB354:" || labelText.ToString()=="Folder:");
            // Retain the separately identified modern cmb13=1148 filename layout.
            // Address-band Edit controls never have this ancestry.

            IntPtr parent=GetParent(child);
            for(int depth=0; !filename && depth<8 && parent!=IntPtr.Zero && parent!=dialog; depth++) {
                if(!OwnedBy(parent,pid)) break;
                if(GetDlgCtrlID(parent)==1148) filename=true;
                parent=GetParent(parent);
            }
            if(filename) matches.Add(child);
            return true;
        },IntPtr.Zero);
        return matches.ToArray();
    }
    public static void SetFolderPath(IntPtr edit,string value) {
        UIntPtr accepted;
        if(WriteMessage(edit,0x000C,UIntPtr.Zero,value,3,2000,out accepted)==IntPtr.Zero || accepted==UIntPtr.Zero) throw new InvalidOperationException("Owned folder Edit rejected WM_SETTEXT");
        // WM_SETTEXT/WM_GETTEXT are system-marshalled Unicode messages; no remote buffer is needed.
        var observed=new StringBuilder(value.Length+1);
        if(ReadMessage(edit,0x000D,new UIntPtr((uint)observed.Capacity),observed,3,2000,out accepted)==IntPtr.Zero || observed.ToString()!=value)
            throw new InvalidOperationException("Owned folder Edit did not retain the exact fixture path");
    }
    public static void Click(IntPtr dialog,uint pid,int id) {
        var button=GetDlgItem(dialog,id);
        if(button==IntPtr.Zero || !OwnedBy(button,pid) || !IsWindowVisible(button) || !IsWindowEnabled(button) || WindowClass(button)!="Button") throw new InvalidOperationException("Owned dialog button missing or disabled");
        UIntPtr ignored;
        if(PlainMessage(button,0x00F5,UIntPtr.Zero,IntPtr.Zero,3,2000,out ignored)==IntPtr.Zero) throw new InvalidOperationException("Owned button did not respond to BM_CLICK");
    }
}
'@

$mainWindows = [AgentGridPickerProbe]::FindMainWindows($AppProcessId)
if ($mainWindows.Length -ne 1) { throw 'Expected one owned OpenDIY Desktop main window; refusing ambiguous action' }
$mainWindow = $mainWindows[0]

function Assert-ProcessIdentity {
    $observed = Get-Process -Id $AppProcessId -ErrorAction Stop
    $observed.Refresh()
    if ($observed.HasExited -or $observed.StartTime.ToUniversalTime().Ticks -ne $startTicks) { throw 'Target process exited or PID was reused' }
    if ($observed.SessionId -ne $currentSession -or -not [string]::Equals([IO.Path]::GetFullPath($observed.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target process identity changed' }
    if (-not [AgentGridPickerProbe]::IsWindow($mainWindow) -or -not [AgentGridPickerProbe]::OwnedBy($mainWindow, $AppProcessId)) { throw 'Original main window identity changed' }
}
function Assert-DialogIdentity {
    Assert-ProcessIdentity
    if (-not [AgentGridPickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogText)) { throw 'Original owned modern folder dialog identity changed' }
}
function Wait-DialogClosed([int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        Assert-ProcessIdentity
        if (-not [AgentGridPickerProbe]::IsWindow($script:dialog) -or -not [AgentGridPickerProbe]::IsWindowVisible($script:dialog)) { return }
        Assert-DialogIdentity
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Owned modern folder dialog did not close within the bounded wait'
}
$deadline = [DateTime]::UtcNow.AddSeconds(7)
$script:dialog = [IntPtr]::Zero
do {
    Assert-ProcessIdentity
    $matches = [AgentGridPickerProbe]::FindDialogs($AppProcessId, $mainWindow, $ExpectedDialogText)
    if ($matches.Length -gt 1) { throw 'Multiple matching owned modern folder dialogs; refusing ambiguous action' }
    if ($matches.Length -eq 1) { $script:dialog = $matches[0]; break }
    Start-Sleep -Milliseconds 50
} while ([DateTime]::UtcNow -lt $deadline)
if ($script:dialog -eq [IntPtr]::Zero) { throw 'Expected owned modern folder dialog not found; no UI action was sent' }

$actions = New-Object 'System.Collections.Generic.List[string]'
$editId = $null
$primaryFailure = $null
$cleanupFailure = $null
try {
    Assert-DialogIdentity
    if ($Mode -eq 'Select') {
        $edits = [AgentGridPickerProbe]::FolderEdits($script:dialog, $AppProcessId)
        if ($edits.Length -ne 1) { throw 'Expected one owned modern folder Edit; refusing ambiguous action' }
        $editId = [AgentGridPickerProbe]::GetDlgCtrlID($edits[0])
        [AgentGridPickerProbe]::SetFolderPath($edits[0], $ExpectedPath)
        $actions.Add('owned-folder-WM_SETTEXT-verified-Unicode')
        Assert-DialogIdentity
        [AgentGridPickerProbe]::Click($script:dialog, $AppProcessId, 1)
        $actions.Add('owned-IDOK-BM_CLICK')
    } else {
        [AgentGridPickerProbe]::Click($script:dialog, $AppProcessId, 2)
        $actions.Add('owned-IDCANCEL-BM_CLICK')
    }
    Wait-DialogClosed 5
} catch {
    $primaryFailure = $_.Exception.Message
    try {
        Assert-ProcessIdentity
        if ([AgentGridPickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogText)) {
            [AgentGridPickerProbe]::Click($script:dialog, $AppProcessId, 2)
            $actions.Add('failure-cleanup-owned-IDCANCEL')
            Wait-DialogClosed 2
        }
    } catch { $cleanupFailure = $_.Exception.Message }
}
if ($primaryFailure) {
    if ($cleanupFailure) { throw "Picker probe failed: $primaryFailure; cleanup failed: $cleanupFailure" }
    throw "Picker probe failed: $primaryFailure; original owned dialog closed or no longer matches"
}
Assert-ProcessIdentity
[ordered]@{
    schema = 'agent-grid-project-picker-probe-v2'
    mode = $Mode
    processId = $AppProcessId
    processStartedAt = $appProcess.StartTime.ToUniversalTime().ToString('o')
    dialogHandle = $script:dialog.ToInt64().ToString()
    parentHandle = $mainWindow.ToInt64().ToString()
    exactProcessAndDialogVerified = $true
    modernFolderDialogVerified = $true
    directUIHWNDVerified = $true
    dialogClosed = $true
    editControlId = $editId
    actions = @($actions.ToArray())
    selectionResultVerified = $false
    selectionResultOracle = 'Caller must verify returned project cwd and persisted registry; dialog closure alone is not selection proof'
    externalProcessesTouched = $false
} | ConvertTo-Json -Depth 4 -Compress
