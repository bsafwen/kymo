#Requires -Version 7.2
<#
.SYNOPSIS
    Sets up the desktop parts for your account (no admin rights needed):
    - task "Kymo desktop card": the live status card on your desktop (card\KymoCard.exe), started at
      sign-in and restarted within 5 minutes if it ever stops;
    - task "Kymo open dashboards": opens http://127.0.0.1:21223 in your default browser after you sign
      in, as soon as the recorder answers (open-dashboards.ps1, hidden, in Windows PowerShell 5.1).
    Both run only while you are signed in. The card is compiled here from card\KymoCard.cs with the
    C# compiler that ships with Windows. Tasks of earlier versions (named PerfHistory) are removed.

.PARAMETER Remove
    Stop the card and remove both tasks.
#>
[CmdletBinding()]
param([switch]$Remove)
$ErrorActionPreference = 'Stop'

$Here = $PSScriptRoot
$ps51 = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$conhost = "$env:SystemRoot\System32\conhost.exe"
$exe = Join-Path $Here 'card\KymoCard.exe'
$user = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$cardTask = 'Kymo desktop card'
$openTask = 'Kymo open dashboards'
# Earlier versions' tasks, when the program was called PerfHistory (and the stack before it).
$earlierTasks = 'PerfHistory desktop widget', 'PerfHistory open dashboards', 'PerfHistory open Grafana', 'PerfHistory desktop status'

# Copies of this card in this session, started by the task or by hand (and an earlier version's).
function Stop-CardProcess {
    $session = (Get-Process -Id $PID).SessionId
    Get-Process KymoCard -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session -and $_.Path -eq $exe } |
        ForEach-Object { $_.Kill(); $_.WaitForExit(5000) | Out-Null }
}
function Remove-Earlier {
    foreach ($t in $earlierTasks) {
        if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {
            Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $t -Confirm:$false
        }
    }
    $session = (Get-Process -Id $PID).SessionId
    Get-Process PerfWidget -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq $session } | ForEach-Object { $_.Kill(); $_.WaitForExit(5000) | Out-Null }
}

if ($Remove) {
    # The tasks go first, so nothing can start the card again while it is being stopped.
    if (Get-ScheduledTask -TaskName $cardTask -ErrorAction SilentlyContinue) { Stop-ScheduledTask -TaskName $cardTask }
    foreach ($t in $cardTask, $openTask) { Unregister-ScheduledTask -TaskName $t -Confirm:$false -ErrorAction SilentlyContinue }
    Stop-CardProcess
    Remove-Earlier
    Write-Host 'Desktop card stopped and both tasks removed.'
    return
}

Remove-Earlier

# Build next to the running card: it is replaced only once the new one has compiled.
& $ps51 -NoProfile -NonInteractive -ExecutionPolicy Bypass -File (Join-Path $Here 'card\build.ps1') -Out "$exe.new"
if ($LASTEXITCODE -ne 0) { throw "building the card failed ($LASTEXITCODE)" }

# Swap the program with the task disabled, so its 5-minute trigger cannot start the old one in between.
$existing = Get-ScheduledTask -TaskName $cardTask -ErrorAction SilentlyContinue
if ($existing) { Disable-ScheduledTask -TaskName $cardTask | Out-Null; Stop-ScheduledTask -TaskName $cardTask }
try {
    Stop-CardProcess
    Move-Item -Force "$exe.new" $exe
}
catch {
    if ($existing) { Enable-ScheduledTask -TaskName $cardTask | Out-Null }
    throw
}

$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited

# Card: at sign-in, and a check every 5 minutes that starts it again if it stopped (while it runs,
# IgnoreNew makes those checks do nothing). No time limit. Priority 5 instead of the default 7: normal CPU
# and I/O priority, because the card must stay live when the PC is overloaded, which is when it matters
# (it costs about 0.3 % of a core). Priority 5 still means memory priority 4: the card sets its own to normal.
$atLogon = New-ScheduledTaskTrigger -AtLogOn -User $user
$every5 = New-ScheduledTaskTrigger -Once -At (Get-Date).Date -RepetitionInterval (New-TimeSpan -Minutes 5)
$cardSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -DontStopOnIdleEnd `
    -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 5
Register-ScheduledTask -TaskName $cardTask -Action (New-ScheduledTaskAction -Execute $exe -WorkingDirectory (Split-Path $exe)) `
    -Trigger $atLogon, $every5 -Principal $principal -Settings $cardSettings `
    -Description "Kymo: the live performance status card on the desktop ($(Join-Path (Split-Path $Here) 'README.md'))." -Force | Out-Null

$t = Get-ScheduledTask -TaskName $cardTask
$rep = @($t.Triggers | Where-Object { $_.Repetition.Interval })[0].Repetition
if ($rep.Interval -ne 'PT5M' -or $rep.Duration) { throw "card task repeats $($rep.Interval) for '$($rep.Duration)', expected every 5 minutes indefinitely" }
if ($t.Settings.ExecutionTimeLimit -ne 'PT0S') { throw "card task has a time limit ($($t.Settings.ExecutionTimeLimit))" }
if (-not $t.Settings.Enabled) { throw 'card task is disabled' }

# The dashboards in the browser after sign-in.
$openAt = New-ScheduledTaskTrigger -AtLogOn -User $user
$openAt.Delay = 'PT15S'
# Priority 5: the browser it starts inherits the CPU priority (the default, 7, is below normal);
# open-dashboards.ps1 sets the memory priority to normal itself.
$openSettings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5) -MultipleInstances IgnoreNew -Priority 5
$openAction = New-ScheduledTaskAction -Execute $conhost -Argument "--headless `"$ps51`" -NoProfile -NonInteractive -ExecutionPolicy Bypass -File `"$Here\open-dashboards.ps1`""
Register-ScheduledTask -TaskName $openTask -Action $openAction -Trigger $openAt -Principal $principal -Settings $openSettings `
    -Description "Kymo: opens the performance dashboards after sign-in ($(Join-Path (Split-Path $Here) 'README.md'))." -Force | Out-Null

# Start the card now and check that it stays up.
Start-ScheduledTask -TaskName $cardTask
Start-Sleep -Seconds 5
$running = Get-Process KymoCard -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $exe }
if ((Get-ScheduledTask -TaskName $cardTask).State -ne 'Running' -or -not $running) {
    throw "the card did not stay up; see $env:LOCALAPPDATA\Kymo\card.log"
}
Write-Host "Done: the live status card is on your desktop (top right until you drag it elsewhere) and starts at each sign-in; the dashboards open after each sign-in."
Write-Host "Undo with: pwsh -File `"$Here\install-desktop.ps1`" -Remove"
