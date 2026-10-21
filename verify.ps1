#Requires -Version 7.2
<#
.SYNOPSIS
    Health report of Kymo, the performance recorder, without admin rights: the service and its recovery
    settings, the recorder's own checks and settings, how soon after the last boot recording started,
    and every gap in the last 7 days with its cause (PC off, asleep, or not explained). Exit code 0
    when all is well. Run it after a reboot to see that recording came back by itself.
#>
[CmdletBinding()]
param([ValidateRange(1, 3650)][int]$Days = 7)
$ErrorActionPreference = 'Stop'
$Service = 'Kymo'
$Api = 'http://127.0.0.1:21223'
$Log = 'C:\ProgramData\Kymo\history\kymo.log'
$problems = [Collections.Generic.List[string]]::new()
function Line([string]$Label, [string]$Text, [bool]$Good = $true) {
    Write-Host ("  {0,-26} {1}" -f $Label, $Text) -ForegroundColor $(if ($Good) { 'Gray' } else { 'Red' })
    if (-not $Good) { $problems.Add("${Label}: $Text") }
}
# The answer, also when it is an error status with a body (health answers 503 when not current);
# $null only when nothing answers.
function Get-Json([string]$Path) { try { Invoke-RestMethod -Uri "$Api$Path" -NoProxy -TimeoutSec 10 -SkipHttpErrorCheck } catch { $null } }
function Local([double]$Unix) { [DateTimeOffset]::FromUnixTimeSeconds([long]$Unix).LocalDateTime.ToString('yyyy-MM-dd HH:mm') }
function Unix([datetime]$T) { [DateTimeOffset]::new($T).ToUnixTimeSeconds() }

Write-Host "`nService" -ForegroundColor Cyan
$svc = Get-CimInstance Win32_Service -Filter "Name='$Service'"
if (-not $svc) {
    Line 'installed' 'no: run install.ps1' $false
} else {
    Line 'state' $svc.State ($svc.State -eq 'Running')
    $delayed = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\$Service" -ErrorAction SilentlyContinue).DelayedAutostart
    Line 'start' "$($svc.StartMode)$(if ($delayed) { ' (delayed)' })" ($svc.StartMode -eq 'Auto' -and -not $delayed)
    Line 'account' $svc.StartName
    Line 'program' $svc.PathName
    $q = & sc.exe qfailure $Service 2>&1 | Out-String
    $restarts = ([regex]::Matches($q, 'RESTART -- Delay = (\d+)') | ForEach-Object { [int]$_.Groups[1].Value / 1000 }) -join ' s, '
    Line 'restarts after a failure' $(if ($restarts) { "after $restarts s" } else { 'none set' }) ([bool]$restarts)
}

Write-Host "`nRecorder" -ForegroundColor Cyan
$health = Get-Json '/api/health'
$healthText = if (-not $health) { 'no answer' }
    elseif ($health.PSObject.Properties['status']) { "$($health.status), newest sample $(if ($null -ne $health.age_s) { "$([math]::Round($health.age_s, 1)) s old" } else { 'none yet' })" }
    else { 'answer not understood' }
Line 'health' $healthText ($health -and $health.PSObject.Properties['status'] -and $health.status -eq 'ok')
$stats = Get-Json '/api/stats'
if ($stats -and $stats.PSObject.Properties['version']) {
    Line 'version, up for' ("{0}, {1:0.0} h" -f $stats.version, ($stats.uptime_s / 3600))
    Line 'its own cost' ("{0:0.00} % of one core, {1:0} MB, history {2:0.0} MB" -f $stats.cpu_percent_of_one_core.since_start, ($stats.memory_bytes.private / 1MB), ($stats.data_bytes.total / 1MB))
}
$card = Get-Json '/api/card'
if ($card -and $card.PSObject.Properties['status']) {
    $src = if ($card.PSObject.Properties['sources']) { ($card.sources.PSObject.Properties | ForEach-Object { "$($_.Name) $($_.Value)" }) -join ', ' } else { '(no current sample)' }
    Line 'readings' $src (-not ($src -match 'failing'))
    if ($card.PSObject.Properties['error'] -and $card.error) { Line 'writing the history' $card.error.text $false }
}
$set = Get-Json '/api/settings'
$interval = 10
if ($set -and $set.PSObject.Properties['values']) {
    $v = $set.values
    $interval = [int]$v.interval
    Line 'settings' ("every {0} s; kept: samples {1} d, minutes {2} d, hours {3} d, apps {4} d; ceiling {5}" -f $v.interval, $v.keep_raw, $v.keep_min, $v.keep_hour,
        $v.keep_apps, $(if ($v.budget_gb) { "$($v.budget_gb) GB" } else { 'none' }))
}

Write-Host "`nLast boot" -ForegroundColor Cyan
$boot = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime
$bootUnix = Unix $boot
# The first sample after the boot; and whether the history goes back before it at all.
$firstAfter = $null
$r = Get-Json "/api/query?keys=cpu.busy&from=$bootUnix&to=$($bootUnix + 1800)&step=10"
if ($r -and $r.PSObject.Properties['series'] -and $r.series) {
    for ($i = 0; $i -lt $r.series[0].avg.Count; $i++) { if ($null -ne $r.series[0].avg[$i]) { $firstAfter = $r.start + ($i + 1) * $r.step; break } }
}
$before = Get-Json "/api/query?keys=cpu.busy&from=$($bootUnix - 30 * 86400)&to=$bootUnix&step=86400"
$hadBefore = $before -and $before.PSObject.Properties['series'] -and $before.series -and @($before.series[0].avg | Where-Object { $null -ne $_ }).Count
if ($firstAfter) {
    $after = $firstAfter - $bootUnix
    Line 'booted' "$($boot.ToString('yyyy-MM-dd HH:mm:ss')); recording from $([math]::Round($after)) s after" ($after -le 180)
} elseif (-not $hadBefore) {
    Line 'booted' "$($boot.ToString('yyyy-MM-dd HH:mm:ss')), before the recorder was installed: restart the PC to check that recording starts by itself"
} else {
    Line 'booted' "$($boot.ToString('yyyy-MM-dd HH:mm:ss')); no sample in the 30 minutes after it" ((Get-Date) - $boot -lt [TimeSpan]::FromMinutes(3))
}

Write-Host "`nGaps of 2 minutes or more, last $Days days" -ForegroundColor Cyan
# Minutes with and without data, a week per request (the API answers at most about 11,000 points).
$now = [long][Math]::Floor([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() / 60) * 60
$from = $now - $Days * 86400
$minutes = [Collections.Generic.List[object]]::new()   # (end of minute, has data)
$queryFailed = $false
for ($cs = $from; $cs -lt $now; $cs += 7 * 86400) {
    $ce = [Math]::Min($now, $cs + 7 * 86400)
    $r = Get-Json "/api/query?keys=cpu.busy&from=$cs&to=$ce&step=60"
    if (-not $r -or -not $r.PSObject.Properties['series']) { $queryFailed = $true; break }
    if (-not $r.series) { for ($t = $cs + 60; $t -le $ce; $t += 60) { $minutes.Add(@($t, $false)) }; continue }
    $avg = $r.series[0].avg
    for ($i = 0; $i -lt $avg.Count; $i++) { $minutes.Add(@(($r.start + ($i + 1) * $r.step), ($null -ne $avg[$i]))) }
}
# When the PC was off or asleep, from the System log: shutdown to boot, sleep to wake. A boot after
# a crash or a power loss has no shutdown before it: then it counts from the last sample.
$since = (Get-Date).AddDays(-$Days - 1)
function Events([string]$Provider, [int[]]$Id) {
    @(Get-WinEvent -FilterHashtable @{ LogName = 'System'; ProviderName = $Provider; Id = $Id; StartTime = $since } -ErrorAction SilentlyContinue |
        ForEach-Object { Unix $_.TimeCreated } | Sort-Object)
}
$boots = Events 'Microsoft-Windows-Kernel-General' 12
$downs = @(Events 'Microsoft-Windows-Kernel-General' 13) + @(Events 'EventLog' 6006) | Sort-Object
$sleeps = @(Events 'Microsoft-Windows-Kernel-Power' 42, 506) | Sort-Object
$wakes = @(Events 'Microsoft-Windows-Power-Troubleshooter' 1) + @(Events 'Microsoft-Windows-Kernel-Power' 107, 507) | Sort-Object
$spans = [Collections.Generic.List[object]]::new()   # (from or $null, to, cause)
$prev = 0
foreach ($b in $boots) {
    $down = @($downs | Where-Object { $_ -lt $b -and $_ -gt $prev }) | Select-Object -Last 1
    $spans.Add(@($down, $b, $(if ($null -ne $down) { 'PC off or restarting' } else { 'PC off (crash or power loss)' })))
    $prev = $b
}
$prev = 0
foreach ($w in $wakes) {
    $s = @($sleeps | Where-Object { $_ -lt $w -and $_ -gt $prev }) | Select-Object -Last 1
    if ($null -ne $s) { $spans.Add(@($s, $w, 'asleep')) }
    $prev = $w
}
# A gap is explained where such spans cover it, allowing 3 minutes for shutting down and 5 for
# starting again; anything left over is not explained.
function Explain([long]$Gs, [long]$Ge) {
    $parts = @(foreach ($sp in $spans) {
        $a = if ($null -ne $sp[0]) { $sp[0] - 180 } else { $Gs }
        $b = $sp[1] + 300 + $interval
        if ($b -gt $Gs -and $a -lt $Ge -and ($null -ne $sp[0] -or ($sp[1] -ge $Gs -and $sp[1] -le $Ge + 60))) {
            [pscustomobject]@{ a = [Math]::Max($Gs, $a); b = [Math]::Min($Ge, $b); why = $sp[2] }
        }
    }) | Sort-Object a
    $covered = 0; $end = $Gs
    foreach ($p in $parts) { if ($p.b -gt $end) { $covered += $p.b - [Math]::Max($end, $p.a); $end = $p.b } }
    $why = @($parts | ForEach-Object why | Select-Object -Unique) -join ', '
    $left = ($Ge - $Gs) - $covered
    if ($parts -and $left -lt 120) { return @($true, $why) }
    if ($parts) { return @($false, "NOT EXPLAINED for $([math]::Round($left / 60)) min ($why for the rest)") }
    return @($false, 'NOT EXPLAINED')
}
$gaps = 0
if ($queryFailed) {
    Line 'gaps' 'could not read the history' $false
} else {
    $firstData = -1
    for ($i = 0; $i -lt $minutes.Count; $i++) { if ($minutes[$i][1]) { $firstData = $i; break } }
    $i = [Math]::Max(0, $firstData)
    while ($firstData -ge 0 -and $i -lt $minutes.Count - 1) {
        if ($minutes[$i][1]) { $i++; continue }
        $j = $i
        while ($j -lt $minutes.Count -and -not $minutes[$j][1]) { $j++ }
        if ($j -lt $minutes.Count -and $j - $i -ge 2) {
            $gs = $minutes[$i][0] - 60; $ge = $minutes[$j][0] - 60
            $ex = Explain $gs $ge
            $gaps++
            Line (Local $gs) ("{0:0} min: {1}" -f (($ge - $gs) / 60), $ex[1]) $ex[0]
        }
        $i = $j
    }
    if (-not $gaps) { Line 'none' '' }
}

Write-Host "`nRecorder log, last $Days days" -ForegroundColor Cyan
if (Test-Path $Log) {
    $cut = [DateTime]::UtcNow.AddDays(-$Days).ToString('yyyy-MM-dd HH:mm:ss')
    # Failed writes log the system's error text ("... (os error 112)") after what was being written.
    $bad = @(Get-Content $Log | Where-Object { $_ -ge $cut -and $_ -match 'panic|has not moved|API stopped|cannot|failing|damaged|unreadable|writing the |tried again|os error' })
    if ($bad) { $bad | Select-Object -Last 10 | ForEach-Object { Line 'log' $_ $false } } else { Line 'problems' 'none' }
    $starts = @(Get-Content $Log | Where-Object { $_ -ge $cut -and $_ -match ' started ' }).Count
    Line 'starts' "$starts"
}

Write-Host "`nDesktop" -ForegroundColor Cyan
foreach ($t in 'Kymo desktop card', 'Kymo open dashboards') {
    $task = Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
    if (-not $task) { Line $t 'missing: run desktop\install-desktop.ps1' $false; continue }
    # What it starts (the card, or the script after -File): gone if the folder it was set up from was deleted.
    $a = @($task.Actions)[0]
    $target = if ($a.Arguments -match '-File\s+"([^"]+)"') { $Matches[1] } else { $a.Execute.Trim('"') }
    $there = Test-Path -LiteralPath $target
    Line $t $(if ($there) { "$($task.State)" } else { "$($task.State), but $target is gone: run desktop\install-desktop.ps1 from a folder you keep" }) $there
}

Write-Host ''
if ($problems.Count) { Write-Host "$($problems.Count) problem(s)." -ForegroundColor Red; exit 1 }
Write-Host 'All well.' -ForegroundColor Green
exit 0
