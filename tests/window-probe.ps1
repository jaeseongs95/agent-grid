[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidateRange(1, 2147483647)][int]$AppProcessId,
    [Parameter(Mandatory = $true)][string]$ExpectedExecutable
)

$ErrorActionPreference = 'Stop'
$expectedPath = [IO.Path]::GetFullPath($ExpectedExecutable)
if (-not [IO.Path]::IsPathRooted($ExpectedExecutable) -or -not (Test-Path -LiteralPath $expectedPath -PathType Leaf)) {
    throw 'Expected executable must be an existing absolute file'
}
$appProcess = Get-Process -Id $AppProcessId -ErrorAction Stop
$appProcess.Refresh()
$startTicks = $appProcess.StartTime.ToUniversalTime().Ticks
$currentSession = (Get-Process -Id $PID).SessionId
if ($appProcess.StartTime.ToUniversalTime() -lt [DateTime]::UtcNow.AddMinutes(-20)) { throw 'Refusing a process older than 20 minutes; launch a fresh isolated app' }
$windowHandle = $appProcess.MainWindowHandle
if ($windowHandle -eq [IntPtr]::Zero) { throw 'Target has no visible main window handle' }
if ($appProcess.MainWindowTitle -ne 'OpenDIY Desktop') { throw 'Unexpected main window title' }

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class AgentGridWindowProbe {
    [DllImport("user32.dll", EntryPoint="PostMessageW", ExactSpelling=true, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool PostMessageW(IntPtr hwnd, uint message, UIntPtr wParam, IntPtr lParam);
    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool IsWindow(IntPtr hwnd);
    [DllImport("user32.dll", EntryPoint="ShowWindowAsync", ExactSpelling=true, SetLastError=true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool ShowWindowAsync(IntPtr hwnd, int command);
    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool IsIconic(IntPtr hwnd);
    [DllImport("kernel32.dll", EntryPoint="SetLastError", ExactSpelling=true)]
    private static extern void ClearNativeLastError(uint error);
    public static int RequestShow(IntPtr hwnd, int command, out bool started) {
        ClearNativeLastError(0);
        started = ShowWindowAsync(hwnd, command);
        // The API documents its return value, not a last-error contract.
        // Capture this immediately as a diagnostic, never as proof of success.
        return Marshal.GetLastWin32Error();
    }
    [DllImport("user32.dll", SetLastError=true)]
    public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
}
'@

function Assert-TargetIdentity {
    $observedProcess = Get-Process -Id $AppProcessId -ErrorAction Stop
    $observedProcess.Refresh()
    if ($observedProcess.HasExited -or $observedProcess.StartTime.ToUniversalTime().Ticks -ne $startTicks) { throw 'Target process exited or PID was reused' }
    if ($observedProcess.SessionId -ne $currentSession) { throw 'Target is in another Windows session' }
    if (-not [string]::Equals([IO.Path]::GetFullPath($observedProcess.Path), $expectedPath, [StringComparison]::OrdinalIgnoreCase)) { throw 'Target executable path does not match' }
    if (-not [AgentGridWindowProbe]::IsWindow($windowHandle)) { throw 'Original window handle was destroyed' }
    [uint32]$ownerProcessId = 0
    [void][AgentGridWindowProbe]::GetWindowThreadProcessId($windowHandle, [ref]$ownerProcessId)
    if ($ownerProcessId -ne $AppProcessId) { throw 'Window handle belongs to another process' }
}

function Wait-Visibility([bool]$ExpectedVisible) {
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Assert-TargetIdentity
        if ([AgentGridWindowProbe]::IsWindowVisible($windowHandle) -eq $ExpectedVisible) { return }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    $currentVisible = [AgentGridWindowProbe]::IsWindowVisible($windowHandle)
    $requestDetail = if ($restoreApi) { $restoreApi | ConvertTo-Json -Compress } else { 'none' }
    throw "Window visibility timeout: stage=$probeStage expectedVisible=$ExpectedVisible currentVisible=$currentVisible closePosted=$closePosted hidden=$hidden restored=$restored hwnd=$($windowHandle.ToInt64()) processId=$AppProcessId restoreApi=$requestDetail"
}

Assert-TargetIdentity
if (-not [AgentGridWindowProbe]::IsWindowVisible($windowHandle)) { throw 'Target main window must initially be visible' }
$closePosted = $false
$hidden = $false
$restored = $false
$probeStage = 'before-close'
$primaryFailure = $null
$restoreFailure = $null
$restoreApi = $null
try {
    # WM_CLOSE follows the app's normal native close handler. No process is killed.
    Assert-TargetIdentity
    if (-not [AgentGridWindowProbe]::PostMessageW($windowHandle, 0x0010, [UIntPtr]::Zero, [IntPtr]::Zero)) {
        throw (New-Object ComponentModel.Win32Exception([Runtime.InteropServices.Marshal]::GetLastWin32Error()))
    }
    $closePosted = $true
    $probeStage = 'wait-hidden'
    Wait-Visibility $false
    $hidden = $true
} catch {
    $primaryFailure = "stage=$probeStage $($_.Exception.Message)"
} finally {
    # Restore only the same verified process/window, including a failed hide wait.
    if ($closePosted) {
        try {
            $probeStage = 'restore-identity'
            Assert-TargetIdentity
            # ShowWindow's first-call STARTUPINFO behavior can interact with this
            # hidden helper process. Async posts to the target window's queue.
            # SW_RESTORE is appropriate for an iconic window; otherwise SW_SHOW.
            $iconic = [AgentGridWindowProbe]::IsIconic($windowHandle)
            $showCommand = if ($iconic) { 9 } else { 5 }
            $showName = if ($iconic) { 'SW_RESTORE' } else { 'SW_SHOW' }
            [bool]$requestStarted = $false
            $probeStage = 'restore-request'
            $requestLastError = [AgentGridWindowProbe]::RequestShow($windowHandle, $showCommand, [ref]$requestStarted)
            $restoreApi = [ordered]@{ api = 'ShowWindowAsync'; command = $showCommand; commandName = $showName; isIconicBefore = $iconic; started = $requestStarted; lastError = $requestLastError; lastErrorDiagnosticOnly = $true }
            if (-not $requestStarted) {
                $requestDetail = $restoreApi | ConvertTo-Json -Compress
                throw "ShowWindowAsync did not start the show operation: $requestDetail"
            }
            $probeStage = 'wait-restored'
            Wait-Visibility $true
            $restored = $true
        } catch {
            $restoreFailure = "stage=$probeStage $($_.Exception.Message)"
        }
    }
}
if ($primaryFailure) {
    if ($restoreFailure) { throw "Primary window probe failure: $primaryFailure; Additional restore failure: $restoreFailure" }
    throw "Primary window probe failure: $primaryFailure; restorationCompleted=$restored"
}
if ($restoreFailure) { throw "Restore window probe failure: $restoreFailure; hiddenBeforeRestore=$hidden" }
Assert-TargetIdentity
[ordered]@{
    processId = $AppProcessId
    handle = $windowHandle.ToInt64().ToString()
    wmClosePosted = $closePosted
    hidden = $hidden
    restored = $restored
    restoreApi = $restoreApi
    sameHandle = $true
    processIdentityPreserved = $true
    processStartedAt = (Get-Process -Id $AppProcessId).StartTime.ToUniversalTime().ToString('o')
    trayIconClickVerified = $false
} | ConvertTo-Json -Compress
