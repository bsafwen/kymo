#Requires -Version 7.2
<#
.SYNOPSIS
    Proves that the Kymo service comes back by itself: kills its process (Windows must start
    it again, as its recovery actions say), then stops and starts it. Run elevated. Expect a few
    seconds of missing samples per step. Exit code 0 when every step passed.

    Windows counts every kill as a failure for a day: the first is restarted after 5 s, the second
    after 10 s, any later one after 60 s. Recording then resumes at the next sample.
#>
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script elevated.'
}
$Service = 'Kymo'
$Api = 'http://127.0.0.1:21223'
$failed = 0

function Healthy { try { (Invoke-RestMethod -Uri "$Api/api/health" -NoProxy -TimeoutSec 3).status -eq 'ok' } catch { $false } }
function Wait-Healthy([int]$Seconds) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
        if (Healthy) { return $sw.Elapsed.TotalSeconds }
        Start-Sleep -Milliseconds 500
    }
    return $null
}
function Report([string]$What, $Seconds, [int]$Limit) {
    if ($null -ne $Seconds -and $Seconds -le $Limit) {
        Write-Host ("  ok    {0}: recording again after {1:0.0} s" -f $What, $Seconds) -ForegroundColor Green
    } else {
        Write-Host "  FAIL  ${What}: not recording within $Limit s" -ForegroundColor Red
        $script:failed++
    }
}

if (-not (Get-Service $Service -ErrorAction SilentlyContinue)) { throw "service $Service not installed" }
if (-not (Wait-Healthy 60)) { throw 'the recorder is not healthy before the test' }
# The first sample after a start comes at the next multiple of the sample interval.
$interval = try { [int](Invoke-RestMethod -Uri "$Api/api/settings" -NoProxy -TimeoutSec 5).values.interval } catch { 10 }
if ($interval -lt 1) { $interval = 10 }
$startLimit = 30 + $interval

# 1. A crash: Windows restarts the service after 5 s, 10 s or 60 s (recovery actions; see above).
$procId = (Get-CimInstance Win32_Service -Filter "Name='$Service'").ProcessId
Stop-Process -Id $procId -Force
Start-Sleep -Seconds 1
Report 'killed process' (Wait-Healthy (60 + $startLimit)) (60 + $startLimit)
$newId = (Get-CimInstance Win32_Service -Filter "Name='$Service'").ProcessId
if ($newId -eq $procId -or $newId -eq 0) { Write-Host '  FAIL  the service did not start a new process' -ForegroundColor Red; $failed++ }

# 2. A clean stop and start.
$sw = [Diagnostics.Stopwatch]::StartNew()
Stop-Service $Service
$stop = $sw.Elapsed.TotalSeconds
if ($stop -le 15) { Write-Host ("  ok    stop answered in {0:0.0} s" -f $stop) -ForegroundColor Green } else { Write-Host "  FAIL  stop took $stop s" -ForegroundColor Red; $failed++ }
Start-Service $Service
Report 'stop and start' (Wait-Healthy $startLimit) $startLimit

# 3. Recovery actions as configured.
$q = & sc.exe qfailure $Service | Out-String
if ($q -match 'RESTART -- Delay = 5000' -and $q -match 'RESTART -- Delay = 60000') { Write-Host '  ok    recovery actions: restart after 5 s, 10 s, then every minute' -ForegroundColor Green }
else { Write-Host "  FAIL  recovery actions:`n$q" -ForegroundColor Red; $failed++ }

exit $failed
