[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 2147483647)][int]$AppProcessId,
    [Parameter(Mandatory = $true)][string]$ExpectedExecutable,
    [Parameter(Mandatory = $true)][ValidateLength(1, 200)][string]$ExpectedDialogTitle,
    [Parameter(Mandatory = $true)][ValidateSet('Select', 'Cancel')][string]$Mode,
    [string]$ExpectedPath
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding=$OutputEncoding=[Text.UTF8Encoding]::new($false)
if (-not [IO.Path]::IsPathRooted($ExpectedExecutable)) { throw 'Expected executable must be absolute' }
$expectedExecutablePath = [IO.Path]::GetFullPath($ExpectedExecutable)
if (-not (Test-Path -LiteralPath $expectedExecutablePath -PathType Leaf) -or [IO.Path]::GetFileName($expectedExecutablePath) -ne 'opendiy-desktop.exe') { throw 'Expected executable must be an existing opendiy-desktop.exe' }
if ($Mode -eq 'Select') {
    if (-not $ExpectedPath -or -not [IO.Path]::IsPathRooted($ExpectedPath) -or -not (Test-Path -LiteralPath $ExpectedPath -PathType Leaf)) { throw 'Select mode requires an existing absolute file' }
    $ExpectedPath = [IO.Path]::GetFullPath($ExpectedPath)
}
$appProcess = Get-Process -Id $AppProcessId -ErrorAction Stop
$appProcess.Refresh()
$startTicks = $appProcess.StartTime.ToUniversalTime().Ticks
$currentSession = (Get-Process -Id $PID).SessionId
if ($appProcess.StartTime.ToUniversalTime() -lt [DateTime]::UtcNow.AddMinutes(-20)) { throw 'Refusing a process older than twenty minutes' }
if ($appProcess.SessionId -ne $currentSession) { throw 'Target is in another Windows session' }
if (-not [string]::Equals([IO.Path]::GetFullPath($appProcess.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target executable path does not match' }
# 활성 파일 선택창도 MainWindowHandle로 반환될 수 있으므로 소유한 주 창을 별도로 찾는다.

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class AgentGridExecutablePickerProbe {
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
    private static extern IntPtr WriteMessage(IntPtr hwnd, uint message, UIntPtr wParam, string text, uint flags, uint timeout, out UIntPtr result);
    [DllImport("user32.dll", EntryPoint="SendMessageTimeoutW", ExactSpelling=true, SetLastError=true)]
    private static extern IntPtr PlainMessage(IntPtr hwnd, uint message, UIntPtr wParam, IntPtr data, uint flags, uint timeout, out UIntPtr result);
    private static string WindowClass(IntPtr hwnd) { var text=new StringBuilder(256); GetClassNameW(hwnd,text,text.Capacity); return text.ToString(); }
    public static bool OwnedBy(IntPtr hwnd, uint pid) { uint observed; GetWindowThreadProcessId(hwnd,out observed); return observed==pid; }
    public static bool IsExpectedDialog(IntPtr hwnd, uint pid, IntPtr owner, string title) {
        if (!IsWindow(hwnd) || !IsWindowVisible(hwnd) || !OwnedBy(hwnd,pid) || WindowClass(hwnd)!="#32770" || GetWindow(hwnd,4)!=owner) return false;
        var text=new StringBuilder(256); GetWindowTextW(hwnd,text,text.Capacity); return text.ToString()==title;
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
    public static IntPtr[] FilenameEdits(IntPtr dialog, uint pid) {
        var matches=new List<IntPtr>(); int visited=0;
        EnumChildWindows(dialog,delegate(IntPtr child,IntPtr unused) {
            if(++visited>512) return false;
            if(!OwnedBy(child,pid) || !IsWindowVisible(child) || !IsWindowEnabled(child) || WindowClass(child)!="Edit") return true;
            // edt1=1152 (classic) or the Edit inside cmb13=1148 (Explorer).
            // The address-bar Edit can also be 1001; its ancestry must not match.
            bool filename=GetDlgCtrlID(child)==1152;
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
    public static void SetFilename(IntPtr edit,string value) {
        UIntPtr accepted;
        if(WriteMessage(edit,0x000C,UIntPtr.Zero,value,3,2000,out accepted)==IntPtr.Zero || accepted==UIntPtr.Zero) throw new InvalidOperationException("Owned filename Edit rejected WM_SETTEXT");
    }
    public static void Click(IntPtr dialog,uint pid,int id) {
        var button=GetDlgItem(dialog,id);
        if(button==IntPtr.Zero || !OwnedBy(button,pid) || !IsWindowVisible(button) || !IsWindowEnabled(button) || WindowClass(button)!="Button") throw new InvalidOperationException("Owned dialog button missing or disabled");
        UIntPtr ignored;
        if(PlainMessage(button,0x00F5,UIntPtr.Zero,IntPtr.Zero,3,2000,out ignored)==IntPtr.Zero) throw new InvalidOperationException("Owned button did not respond to BM_CLICK");
    }
}
'@

$mainWindows = [AgentGridExecutablePickerProbe]::FindMainWindows($AppProcessId)
if ($mainWindows.Length -ne 1) { throw 'Expected one owned OpenDIY Desktop main window; refusing ambiguous action' }
$mainWindow = $mainWindows[0]

function Assert-ProcessIdentity {
    $observed = Get-Process -Id $AppProcessId -ErrorAction Stop
    $observed.Refresh()
    if ($observed.HasExited -or $observed.StartTime.ToUniversalTime().Ticks -ne $startTicks) { throw 'Target process exited or PID was reused' }
    if ($observed.SessionId -ne $currentSession -or -not [string]::Equals([IO.Path]::GetFullPath($observed.Path), $expectedExecutablePath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target process identity changed' }
    if (-not [AgentGridExecutablePickerProbe]::IsWindow($mainWindow) -or -not [AgentGridExecutablePickerProbe]::OwnedBy($mainWindow, $AppProcessId)) { throw 'Original main window identity changed' }
}
function Assert-DialogIdentity {
    Assert-ProcessIdentity
    if (-not [AgentGridExecutablePickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogTitle)) { throw 'Original owned executable dialog identity changed' }
}
function Wait-DialogClosed([int]$Seconds) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        Assert-ProcessIdentity
        if (-not [AgentGridExecutablePickerProbe]::IsWindow($script:dialog) -or -not [AgentGridExecutablePickerProbe]::IsWindowVisible($script:dialog)) { return }
        Assert-DialogIdentity
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Owned executable dialog did not close within the bounded wait'
}
$deadline = [DateTime]::UtcNow.AddSeconds(7)
$script:dialog = [IntPtr]::Zero
do {
    Assert-ProcessIdentity
    $matches = [AgentGridExecutablePickerProbe]::FindDialogs($AppProcessId, $mainWindow, $ExpectedDialogTitle)
    if ($matches.Length -gt 1) { throw 'Multiple matching owned executable dialogs; refusing ambiguous action' }
    if ($matches.Length -eq 1) { $script:dialog = $matches[0]; break }
    Start-Sleep -Milliseconds 50
} while ([DateTime]::UtcNow -lt $deadline)
if ($script:dialog -eq [IntPtr]::Zero) { throw 'Expected owned executable dialog not found; no UI action was sent' }

$actions = New-Object 'System.Collections.Generic.List[string]'
$editId = $null
$primaryFailure = $null
$cleanupFailure = $null
try {
    Assert-DialogIdentity
    if ($Mode -eq 'Select') {
        $edits = [AgentGridExecutablePickerProbe]::FilenameEdits($script:dialog, $AppProcessId)
        if ($edits.Length -ne 1) { throw 'Expected one owned filename Edit; refusing ambiguous action' }
        $editId = [AgentGridExecutablePickerProbe]::GetDlgCtrlID($edits[0])
        [AgentGridExecutablePickerProbe]::SetFilename($edits[0], $ExpectedPath)
        $actions.Add('owned-filename-WM_SETTEXT')
        Assert-DialogIdentity
        [AgentGridExecutablePickerProbe]::Click($script:dialog, $AppProcessId, 1)
        $actions.Add('owned-IDOK-BM_CLICK')
    } else {
        [AgentGridExecutablePickerProbe]::Click($script:dialog, $AppProcessId, 2)
        $actions.Add('owned-IDCANCEL-BM_CLICK')
    }
    Wait-DialogClosed 5
} catch {
    $primaryFailure = $_.Exception.Message
    try {
        Assert-ProcessIdentity
        if ([AgentGridExecutablePickerProbe]::IsExpectedDialog($script:dialog, $AppProcessId, $mainWindow, $ExpectedDialogTitle)) {
            [AgentGridExecutablePickerProbe]::Click($script:dialog, $AppProcessId, 2)
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
    schema = 'agent-grid-executable-picker-probe-v2'
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
    selectionResultOracle = 'Caller must verify returned executable path, persisted settings and live PTY identities'
    externalProcessesTouched = $false
} | ConvertTo-Json -Depth 4 -Compress
