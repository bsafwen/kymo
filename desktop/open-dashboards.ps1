<#
.SYNOPSIS
    Opens the performance dashboards in your default browser after you sign in, once the recorder
    answers (after 3 minutes it opens anyway). Run by the task "Kymo open dashboards"
    (install-desktop.ps1).
#>
$ErrorActionPreference = 'Stop'
$url = 'http://127.0.0.1:21223/'
$ok = $false
$deadline = (Get-Date).AddMinutes(3)
while (-not $ok -and (Get-Date) -lt $deadline) {
    try {
        $req = [System.Net.HttpWebRequest]::Create("${url}api/health")
        $req.Proxy = $null; $req.Timeout = 5000
        $resp = $req.GetResponse()
        $ok = [int]$resp.StatusCode -eq 200
        $resp.Close()
    } catch { }
    if (-not $ok) { Start-Sleep -Seconds 5 }
}
# The browser inherits this process's priorities: never start it below normal. The task gives normal CPU
# priority but memory priority 4; 0 = ProcessMemoryPriority, 5 = MEMORY_PRIORITY_NORMAL.
[Diagnostics.Process]::GetCurrentProcess().PriorityClass = 'Normal'
try {
    Add-Type -Name MemPriority -Namespace Kymo -MemberDefinition @'
[DllImport("kernel32.dll")] public static extern IntPtr GetCurrentProcess();
[DllImport("kernel32.dll")] public static extern bool SetProcessInformation(IntPtr process, int infoClass, ref uint info, int size);
'@
    $normal = [uint32]5
    [void][Kymo.MemPriority]::SetProcessInformation([Kymo.MemPriority]::GetCurrentProcess(), 0, [ref]$normal, 4)
} catch { }
Start-Process $url
