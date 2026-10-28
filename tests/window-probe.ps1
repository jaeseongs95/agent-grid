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
if ($appProcess.MainWindowTitle -ne 'Agent Grid') { throw 'Unexpected main window title' }

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
    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool ShowWindow(IntPtr hwnd, int command);
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
    throw 'Window visibility did not reach the expected state'
}

Assert-TargetIdentity
if (-not [AgentGridWindowProbe]::IsWindowVisible($windowHandle)) { throw 'Target main window must initially be visible' }
$closePosted = $false
$hidden = $false
$restored = $false
try {
    # WM_CLOSE follows the app's normal native close handler. No process is killed.
    Assert-TargetIdentity
    if (-not [AgentGridWindowProbe]::PostMessageW($windowHandle, 0x0010, [UIntPtr]::Zero, [IntPtr]::Zero)) {
        throw (New-Object ComponentModel.Win32Exception([Runtime.InteropServices.Marshal]::GetLastWin32Error()))
    }
    $closePosted = $true
    Wait-Visibility $false
    $hidden = $true
} finally {
    # Restore only the same verified process/window, including a failed hide wait.
    if ($closePosted) {
        Assert-TargetIdentity
        [void][AgentGridWindowProbe]::ShowWindow($windowHandle, 5)
        Wait-Visibility $true
        $restored = $true
    }
}
Assert-TargetIdentity
[ordered]@{
    processId = $AppProcessId
    handle = $windowHandle.ToInt64().ToString()
    wmClosePosted = $closePosted
    hidden = $hidden
    restored = $restored
    sameHandle = $true
    processIdentityPreserved = $true
    processStartedAt = (Get-Process -Id $AppProcessId).StartTime.ToUniversalTime().ToString('o')
    trayIconClickVerified = $false
} | ConvertTo-Json -Compress
