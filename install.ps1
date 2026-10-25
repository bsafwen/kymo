#Requires -Version 7.2
<#
.SYNOPSIS
    Installs or updates Kymo, the performance recorder of this PC, as the Windows service "Kymo". It
    starts at boot, records into C:\ProgramData\Kymo\history (every 10 s by default; the
    dashboards' Settings page changes that and how long the history is kept) and serves the
    dashboards and the desktop card on http://127.0.0.1:21223.

    Run it as yourself, not elevated: it builds and tests the program (recorder, Rust) as you,
    then asks once for administrator rights (UAC) to install it, then sets up the desktop card and the
    dashboards at sign-in (desktop\install-desktop.ps1). A copy of the card's program goes next to the
    recorder, where only administrators can change it: the process list runs it for what it does as
    administrator (its right-click menu, when Windows refuses the card's own rights).

    The service runs as LocalService. If a reading the recorder had before (CPU power, the GPU) is
    missing under that account, the installer moves it to LocalSystem and says so. The readings and
    the account are kept in C:\ProgramData\Kymo\install-state.json for the next run.

    An install made under the program's earlier name, PerfHistory, is taken over: its history is
    copied, and its service and folders go once Kymo has passed its checks (if Kymo fails, the old
    service starts again).

.PARAMETER RemoveOldStack
    Only when migrating from the earlier Grafana-based stack: also remove it (Grafana, VictoriaMetrics,
    windows_exporter, nvidia_gpu_exporter and their watchdog task) once the new service is up and
    checked. Its history in C:\ProgramData\VictoriaMetrics is kept. Do not use it if you run those
    products for anything else.

.PARAMETER Test
    After installing, run test-resilience.ps1: it kills the service and checks that Windows starts
    it again, and that a stop and a start work.

.NOTES
    Re-running it is safe: the service is updated in place and the history kept.
    Log of the elevated part: C:\ProgramData\Kymo\logs\install-<time>.log
#>
[CmdletBinding()]
param(
    [switch]$RemoveOldStack,
    [switch]$Test,
    # Internal: the elevated part.
    [switch]$Elevated,
    [string]$Exe,
    [string]$Sha,
    [string]$Card,
    [string]$CardSha,
    [string]$StatusFile,
    [string]$Expect
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

$Here    = $PSScriptRoot
$Src     = Join-Path $Here 'recorder'
$Service = 'Kymo'
$ProgDir = 'C:\Program Files\Kymo'
$ExeDest = "$ProgDir\kymo.exe"
$CardDest = "$ProgDir\KymoCard.exe"
$PdRoot  = 'C:\ProgramData\Kymo'
$History = "$PdRoot\history"
$Logs    = "$PdRoot\logs"
$Port    = 21223
$Api     = "http://127.0.0.1:$Port"
# The program's earlier name, and where that install (and the Grafana-based stack before it) kept
# its files.
$OldService = 'PerfHistory'
$OldProgDir = 'C:\Program Files\PerfHistory'
$OldPdRoot  = 'C:\ProgramData\PerfHistory'
$OldHistory = "$OldPdRoot\history"

function Get-Json([string]$Path) {
    try { return Invoke-RestMethod -Uri "$Api$Path" -NoProxy -TimeoutSec 5 } catch { return $null }
}

# The keys of the newest sample (none while the recorder has no sample yet, or does not answer).
function Get-Keys {
    $n = Get-Json '/api/now'
    if ($n -and $n.PSObject.Properties['series']) { return @($n.series | ForEach-Object { $_.key }) }
    return @()
}

# ---------------------------------------------------------------- as you
if (-not $Elevated) {
    if (([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run this script as yourself, not elevated: it builds the program as you, then asks for administrator rights.'
    }
    # A release zip carries the program built and tested by the release workflow (bin\kymo.exe): no Rust
    # needed. A clone of the repository builds and tests it here.
    $prebuilt = Join-Path $Here 'bin\kymo.exe'
    if (Test-Path -LiteralPath $prebuilt) {
        $built = $prebuilt
        Write-Host "Installing the program of this release: $built"
    }
    else {
        $built = Join-Path $Src 'target\release\kymo.exe'
        $blocking = Get-Process kymo -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $built }
        if ($blocking) { throw "Stop the program running from $built first (PID $($blocking.Id -join ', ')): run dev servers from a copy." }
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'Rust is needed to build from the source (https://rustup.rs), or install from a release zip, which has the program built.' }
        # From recorder\, so that cargo reads .cargo\config.toml (the C runtime linked in).
        Push-Location $Src
        try {
            & cargo build --release --manifest-path "$Src\Cargo.toml"
            if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
            & cargo test --release --manifest-path "$Src\Cargo.toml" --quiet
            if ($LASTEXITCODE -ne 0) { throw "cargo test failed ($LASTEXITCODE)" }
        }
        finally { Pop-Location }
    }

    # What the recorder running now reads, so the service can be checked against it.
    $keys = Get-Keys
    $expect = @(@('power.package', 'gpu.util', 'cpu.busy', 'mem.available') | Where-Object { $_ -in $keys }) -join ','

    # A copy that the elevated part checks against this hash before it installs it.
    $stage = Join-Path ([IO.Path]::GetTempPath()) "kymo-setup-$PID"
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item $built "$stage\kymo.exe" -Force
    $hash = (Get-FileHash "$stage\kymo.exe" -Algorithm SHA256).Hash
    # The card's program, built here too: the elevated part puts it next to the recorder.
    & (Join-Path $Here 'desktop\card\build.ps1') -Out "$stage\KymoCard.exe" | Out-Null
    $cardHash = (Get-FileHash "$stage\KymoCard.exe" -Algorithm SHA256).Hash
    $status = "$stage\status.json"
    $argList = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Elevated',
        '-Exe', "`"$stage\kymo.exe`"", '-Sha', $hash, '-Card', "`"$stage\KymoCard.exe`"", '-CardSha', $cardHash,
        '-StatusFile', "`"$status`"", '-Expect', "`"$expect`"")
    if ($RemoveOldStack) { $argList += '-RemoveOldStack' }
    if ($Test) { $argList += '-Test' }
    Write-Host 'Asking for administrator rights to install the service...'
    $p = Start-Process -FilePath (Get-Process -Id $PID).Path -Verb RunAs -ArgumentList $argList -Wait -PassThru
    $result = if (Test-Path $status) { Get-Content -Raw $status | ConvertFrom-Json } else { $null }
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
    if (-not $result -or $result.result -ne 'ok') {
        if ($result) { $result | ConvertTo-Json -Depth 5 | Write-Host }
        throw "the elevated install did not finish (exit code $($p.ExitCode)); see $Logs"
    }
    $result | ConvertTo-Json -Depth 5 | Write-Host
    # The desktop card and the dashboards at sign-in, for your account.
    & (Join-Path $Here 'desktop\install-desktop.ps1')
    Write-Host "`nDone: the service Kymo records into $History; dashboards on $Api" -ForegroundColor Green
    exit 0
}

# ---------------------------------------------------------------- elevated
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The elevated part needs administrator rights.'
}
$SidSystem = 'S-1-5-18'; $SidAdmins = 'S-1-5-32-544'; $SidUsers = 'S-1-5-32-545'
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$status = [ordered]@{ started = (Get-Date).ToString('o'); result = 'failed'; account = $null; migrated = $null; checks = $null; old_stack = $null; test = $null; warnings = @() }
$warnings = [Collections.Generic.List[string]]::new()

function Step([string]$Text) { Write-Host "`n=== $Text" -ForegroundColor Cyan }
function Ok([string]$Text) { Write-Host "  ok  $Text" -ForegroundColor Green }
function Warn([string]$Text) { Write-Host "  !!  $Text" -ForegroundColor Yellow; $warnings.Add($Text) }

function Invoke-Native([string]$File, [string[]]$Arguments, [int[]]$OkCodes = @(0)) {
    $out = & $File @Arguments 2>&1 | Out-String
    if ($LASTEXITCODE -notin $OkCodes) { throw "$File $($Arguments -join ' ') failed with exit code $LASTEXITCODE`n$out" }
    return $out
}

function Wait-Until([scriptblock]$Check, [int]$Seconds, [string]$What) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    while ((Get-Date) -lt $deadline) {
        if (& $Check) { return }
        Start-Sleep -Seconds 2
    }
    throw "timed out after $Seconds s waiting for: $What"
}

# Locked: SYSTEM and Administrators full control, Users read; owned by Administrators; no links.
# C:\ProgramData lets any user create folders, so an existing one must not be trusted as it is.
function Protect-Folder([string]$Path) {
    $sd = [Security.AccessControl.DirectorySecurity]::new()
    $sd.SetAccessRuleProtection($true, $false)
    foreach ($r in @(@($SidSystem, 'FullControl'), @($SidAdmins, 'FullControl'), @($SidUsers, 'ReadAndExecute'))) {
        $sd.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new([Security.Principal.SecurityIdentifier]::new($r[0]),
            $r[1], 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    if (Test-Path -LiteralPath $Path) {
        $item = Get-Item -LiteralPath $Path -Force
        if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "$Path is a link or junction: delete it and run again." }
        $owner = (Get-Acl -LiteralPath $Path).GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($owner -notin $SidAdmins, $SidSystem) { throw "$Path is owned by $owner, not by Administrators: delete it and run again." }
        [IO.FileSystemAclExtensions]::SetAccessControl([IO.DirectoryInfo]::new($Path), $sd)
        $links = @(Get-ChildItem -LiteralPath $Path -Recurse -Force -Attributes ReparsePoint -ErrorAction SilentlyContinue)
        if ($links) { throw "$Path contains links or junctions: $($links.FullName -join ', ')" }
    } else {
        [IO.FileSystemAclExtensions]::Create([IO.DirectoryInfo]::new($Path), $sd)
    }
    Invoke-Native icacls.exe @($Path, '/setowner', "*$SidAdmins", '/T', '/C', '/L', '/Q') | Out-Null
}

# Uninstalls the products whose name matches; $true when none is left. Waits up to 2 minutes for
# another Windows Installer operation to end first (it would fail with 1618).
function Remove-Msi([string]$NamePattern) {
    $keys = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*' -ErrorAction SilentlyContinue |
        Where-Object { $_.PSObject.Properties['DisplayName'] -and $_.DisplayName -match $NamePattern }
    $done = $true
    foreach ($k in $keys) {
        $m = $null; $deadline = (Get-Date).AddMinutes(2)
        while ([Threading.Mutex]::TryOpenExisting('Global\_MSIExecute', [ref]$m) -and (Get-Date) -lt $deadline) { $m.Dispose(); Start-Sleep -Seconds 3 }
        if ($m) { $m.Dispose() }
        $p = Start-Process msiexec.exe -ArgumentList '/x', $k.PSChildName, '/qn', '/norestart' -Wait -PassThru
        if ($p.ExitCode -in 0, 1605, 3010) { Ok "uninstalled $($k.DisplayName)" } else { Warn "msiexec /x $($k.DisplayName) exit code $($p.ExitCode)"; $done = $false }
    }
    return $done
}

# Removes a product of the previous stack: its MSI, then its service if one is still there (the MSI
# gone already, or not the one that made it). $true once the service is gone or marked for deletion.
function Remove-Product([string]$Pattern, [string]$ServiceName) {
    if (-not (Remove-Msi $Pattern)) { return $false }
    if (Get-Service $ServiceName -ErrorAction SilentlyContinue) {
        Stop-Service $ServiceName -Force -ErrorAction SilentlyContinue
        $null = & sc.exe delete $ServiceName 2>&1
        if ($LASTEXITCODE -notin 0, 1060, 1072) { Warn "sc delete $ServiceName failed with exit code $LASTEXITCODE"; return $false }
        Ok "service $ServiceName removed"
    }
    return $true
}

# A service marked for deletion: it goes once the last handle to it closes, and cannot start.
function Test-DeletePending([string]$Name) {
    $reg = Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\$Name" -ErrorAction SilentlyContinue
    return [bool]($reg -and $reg.PSObject.Properties['DeleteFlag'] -and $reg.DeleteFlag -eq 1)
}

# Stops a recorder service if it runs. One that disappears as it stops (it was marked for deletion)
# counts as stopped.
function Stop-Recorder([string]$Name, [string]$Done) {
    $s = Get-Service $Name -ErrorAction SilentlyContinue
    if (-not $s -or $s.Status -eq 'Stopped') { return }
    try { Stop-Service $Name -Force } catch { if (Get-Service $Name -ErrorAction SilentlyContinue) { throw } }
    Wait-Until { $x = Get-Service $Name -ErrorAction SilentlyContinue; -not $x -or $x.Status -eq 'Stopped' } 30 "the service $Name to stop"
    Ok $Done
}

# The logs of the install under the earlier name join this install's, under earlier\, once the
# Grafana-based stack's watchdog (which writes there too) is gone. A name already there is left
# where it was rather than overwritten.
function Move-EarlierLogs {
    $from = "$OldPdRoot\logs"
    if (-not (Test-Path -LiteralPath $from) -or (Get-ScheduledTask -TaskPath '\PerfHistory\' -ErrorAction SilentlyContinue)) { return }
    $earlier = Join-Path $Logs 'earlier'
    New-Item -ItemType Directory -Force $earlier | Out-Null
    foreach ($f in Get-ChildItem -LiteralPath $from -Force) {
        if (-not (Test-Path -LiteralPath (Join-Path $earlier $f.Name))) { Move-Item -LiteralPath $f.FullName -Destination $earlier -ErrorAction SilentlyContinue }
    }
}

# The folders of the earlier name, once nothing is left in them.
function Remove-EmptyOldFolders {
    foreach ($d in "$OldPdRoot\logs", $OldProgDir, $OldPdRoot) {
        if ((Test-Path -LiteralPath $d) -and -not (Get-ChildItem -LiteralPath $d -Force)) { Remove-Item -Force -LiteralPath $d }
    }
}

function Write-Status {
    $status.warnings = @($warnings)
    $status.finished = (Get-Date).ToString('o')
    $json = $status | ConvertTo-Json -Depth 5
    if (Test-Path $Logs) { [IO.File]::WriteAllText("$Logs\install-status.json", $json, [Text.UTF8Encoding]::new($false)) }
    if ($StatusFile) { [IO.File]::WriteAllText($StatusFile, $json, [Text.UTF8Encoding]::new($false)) }
}

# Before there is a log folder to write to, a failure still reaches the status file you see.
try {
    New-Item -ItemType Directory -Force $PdRoot | Out-Null
    Protect-Folder $PdRoot
    New-Item -ItemType Directory -Force $Logs | Out-Null
} catch {
    $status.error = "preparing $PdRoot failed: $($_ | Out-String)"
    Write-Host "`nFAILED: $($_.Exception.Message)" -ForegroundColor Red
    Write-Status
    exit 1
}
Start-Transcript -Path "$Logs\install-$stamp.log" | Out-Null
$StateFile = "$PdRoot\install-state.json"
$tookOver = $false
$migratedNow = $false
# There while the history is a copy of the earlier install's and that install has not yet been
# removed (a run cut short in between leaves it): the next run may still go back to that install.
$Uncommitted = "$PdRoot\history.uncommitted"
$pendingAtStart = Test-Path -LiteralPath $Uncommitted
$serviceExisted = [bool](Get-Service $Service -ErrorAction SilentlyContinue)
# The install under the earlier name, if it can run: one marked for deletion, or disabled, is nothing
# to go back to.
$oldUsable = $false
$old = Get-Service $OldService -ErrorAction SilentlyContinue
if ($old) { $oldUsable = -not (Test-DeletePending $OldService) -and $old.StartType -ne 'Disabled' }
try {
    # ------------------------------------------------------------ program
    Step 'Program'
    if ($serviceExisted -and (Test-DeletePending $Service)) {
        throw "the service $Service is being deleted and goes once nothing holds it open: close the Services window (or restart the PC), then run this again"
    }
    if ((Get-FileHash -LiteralPath $Exe -Algorithm SHA256).Hash -ne $Sha) { throw "the staged program does not match the hash of the build" }
    Stop-Recorder $Service 'service stopped for the update'
    # The install under the earlier name: stopped now (it holds the port), removed only once Kymo has
    # passed its checks (it starts again if this install fails); its history is copied below.
    Stop-Recorder $OldService "service $OldService (the earlier name) stopped"
    Get-Process kymo, perfhistory -ErrorAction SilentlyContinue | Where-Object { $_.Path -and $_.Path -ne $ExeDest } |
        ForEach-Object { $_.Kill(); $_.WaitForExit(5000) | Out-Null; Ok "stopped $($_.Path) (PID $($_.Id))" }
    if (-not (Test-Path $ProgDir)) { New-Item -ItemType Directory $ProgDir | Out-Null }
    Copy-Item -LiteralPath $Exe -Destination "$ExeDest.new" -Force
    if ((Get-FileHash -LiteralPath "$ExeDest.new" -Algorithm SHA256).Hash -ne $Sha) { Remove-Item "$ExeDest.new" -Force; throw 'the copied program does not match the build' }
    Move-Item -LiteralPath "$ExeDest.new" -Destination $ExeDest -Force
    Ok "$ExeDest ($Sha)"
    # The card's program for what the process list does as administrator: run from here, where only administrators
    # can change it or add a file next to it (the card itself runs from your folder).
    if ($Card) {
        if ((Get-FileHash -LiteralPath $Card -Algorithm SHA256).Hash -ne $CardSha) { throw 'the staged card program does not match the hash of its build' }
        Copy-Item -LiteralPath $Card -Destination "$CardDest.new" -Force
        if ((Get-FileHash -LiteralPath "$CardDest.new" -Algorithm SHA256).Hash -ne $CardSha) { Remove-Item "$CardDest.new" -Force; throw 'the copied card program does not match its build' }
        Move-Item -LiteralPath "$CardDest.new" -Destination $CardDest -Force
        Ok "$CardDest ($CardSha)"
    }

    # ------------------------------------------------------------ history
    Step 'History'
    # The earlier install's history is copied into a folder of its own and renamed into place once
    # checked, so a copy cut short is never taken for the history: the next run starts it again.
    $staging = "$History.migrating"
    if (Test-Path -LiteralPath $staging) { Remove-Item -Recurse -Force -LiteralPath $staging }
    $have = @(Get-ChildItem $History -Recurse -File -Filter *.phr -ErrorAction SilentlyContinue)
    $oldHave = @(Get-ChildItem $OldHistory -Recurse -File -Filter *.phr -ErrorAction SilentlyContinue).Count
    # A copy from a run that did not finish, while the earlier install is still there (and may have
    # recorded since): copied again, and the copy kept aside, never deleted (Kymo may have recorded
    # into it too).
    $recopy = [bool]($have -and $oldHave -and $pendingAtStart -and $oldUsable)
    if ($recopy) { $have = @() }
    if (-not $have -and $oldHave) {
        $from = @(Get-ChildItem $OldHistory -Recurse -File)
        $rc = (Start-Process robocopy.exe -ArgumentList "`"$OldHistory`"", "`"$staging`"", '/E', '/COPY:DT', '/DCOPY:T', '/R:2', '/W:1', '/NP', '/NFL', '/NDL', '/NJH', '/NJS', '/XJ' -Wait -PassThru -NoNewWindow).ExitCode
        $to = @(Get-ChildItem $staging -Recurse -File -ErrorAction SilentlyContinue)
        $bytesFrom = ($from | ForEach-Object Length | Measure-Object -Sum).Sum; $bytesTo = ($to | ForEach-Object Length | Measure-Object -Sum).Sum
        $bad = if ($rc -ge 8) { "robocopy of the history failed ($rc)" }
            elseif ($to.Count -ne $from.Count -or $bytesTo -ne $bytesFrom) { "history copy incomplete: $($to.Count)/$($from.Count) files, $bytesTo/$bytesFrom bytes" }
        # A copy that failed leaves nothing behind (the earlier install keeps the history).
        if ($bad) { Remove-Item -Recurse -Force -LiteralPath $staging -ErrorAction SilentlyContinue; throw $bad }
        foreach ($n in 'log', 'log.1') {
            $oldLog = Join-Path $staging "perfhistory.$n"
            if (Test-Path -LiteralPath $oldLog) { Rename-Item -LiteralPath $oldLog -NewName "kymo.$n" }
        }
        [IO.File]::WriteAllText($Uncommitted, "copy of $OldHistory, $stamp")
        if ($recopy) {
            $aside = "$History.uncommitted-$stamp"
            Rename-Item -LiteralPath $History -NewName (Split-Path $aside -Leaf)
            Warn "a run that did not finish left a copy of the history, which Kymo may have recorded into since: kept in $aside; the earlier install's history is copied again"
        } elseif (Test-Path -LiteralPath $History) {
            # (It holds no history file: checked above.)
            Remove-Item -Recurse -Force -LiteralPath $History
        }
        Rename-Item -LiteralPath $staging -NewName (Split-Path $History -Leaf)
        $migratedNow = $true
        $status.migrated = "$($to.Count) files, $([math]::Round($bytesTo / 1MB, 1)) MB from $OldHistory"
        Ok $status.migrated
    } else {
        if (-not (Test-Path $History)) { New-Item -ItemType Directory $History | Out-Null }
        $status.migrated = "kept ($($have.Count) files)"
        Ok "history kept: $($have.Count) files"
    }
    # Files copied in inherit the folder's permissions.
    Invoke-Native icacls.exe @("$History\*", '/reset', '/T', '/C', '/L', '/Q') -OkCodes 0, 2 | Out-Null

    # ------------------------------------------------------------ service
    Step 'Service'
    $bin = "`"$ExeDest`" service --data `"$History`" --port $Port"
    if (-not (Get-Service $Service -ErrorAction SilentlyContinue)) {
        New-Service -Name $Service -BinaryPathName $bin -DisplayName 'Kymo recorder' -StartupType Automatic `
            -Description 'Kymo: records the performance of this PC and serves the dashboards on http://127.0.0.1:21223 (settings there).' | Out-Null
        Ok 'service created'
    } else {
        Invoke-Native sc.exe @('config', $Service, 'binPath=', $bin, 'start=', 'auto') | Out-Null
        Ok 'service updated'
    }
    Set-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\$Service" -Name DelayedAutostart -Type DWord -Value 0
    Invoke-Native sc.exe @('sidtype', $Service, 'unrestricted') | Out-Null
    # Restarted 5 s after a crash or a hang, then 10 s, then every minute (the last action repeats).
    Invoke-Native sc.exe @('failure', $Service, 'reset=', '86400', 'actions=', 'restart/5000/restart/10000/restart/60000') | Out-Null
    Invoke-Native sc.exe @('failureflag', $Service, '1') | Out-Null
    Invoke-Native icacls.exe @($History, '/grant', "NT SERVICE\${Service}:(OI)(CI)M", '/L', '/Q') | Out-Null

    # The readings the service must have: those of the recorder that answered before UAC, and those
    # an earlier install kept. When neither is known, what this PC offers (RAPL counters, NVML) is
    # wanted too, but its absence under both accounts is only a warning. Readings switched off in the
    # settings are not asked for.
    $required = [Collections.Generic.List[string]]::new()
    $Expect -split ',' | Where-Object { $_ } | ForEach-Object { $required.Add($_) }
    $saved = $null
    # (The earlier install's, when this is the first install under the name Kymo.)
    $stateIn = if (Test-Path $StateFile) { $StateFile } elseif (Test-Path "$OldPdRoot\install-state.json") { "$OldPdRoot\install-state.json" } else { $null }
    if ($stateIn) { try { $saved = Get-Content -Raw $stateIn | ConvertFrom-Json } catch { Warn "ignored $stateIn ($($_.Exception.Message))" } }
    if ($saved -and $saved.PSObject.Properties['readings']) { @($saved.readings) | Where-Object { $_ -and $_ -notin $required } | ForEach-Object { $required.Add($_) } }
    $wanted = @('cpu.busy', 'mem.available')
    if (Get-Counter -ListSet 'Energy Meter' -ErrorAction SilentlyContinue | Where-Object { $_.PathsWithInstances -match 'RAPL_Package0_PKG' }) { $wanted += 'power.package' }
    if (Test-Path "$env:SystemRoot\System32\nvml.dll") { $wanted += 'gpu.util' }
    $conf = Join-Path $History 'settings.conf'
    $off = @()
    # The sample interval sets how long the first samples take: a start's first sample is only a
    # baseline, the first kept one comes an interval later.
    $interval = 10
    if (Test-Path $conf) {
        if (Select-String -LiteralPath $conf -Pattern '^\s*gpu\s*=\s*(false|0|off)\s*$' -Quiet) { $off += 'gpu.util' }
        if (Select-String -LiteralPath $conf -Pattern '^\s*power\s*=\s*(false|0|off)\s*$' -Quiet) { $off += 'power.package' }
        $m = @(Select-String -LiteralPath $conf -Pattern '^\s*interval\s*=\s*(\d+)\s*$')
        if ($m) { $interval = [Math]::Min(60, [Math]::Max(1, [int]$m[0].Matches[0].Groups[1].Value)) }
    }
    $firstWait = 2 * $interval + 30
    $required = @($required | Where-Object { $_ -notin $off })
    $wanted = @($wanted | Where-Object { $_ -notin $off -and $_ -notin $required })

    function Test-Service([string]$Account) {
        Invoke-Native sc.exe @('config', $Service, 'obj=', $Account, 'password=', '') | Out-Null
        try {
            if ((Get-Service $Service).Status -ne 'Stopped') { Stop-Service $Service -Force; Wait-Until { (Get-Service $Service).Status -eq 'Stopped' } 30 'stop' }
            Start-Service $Service
            Wait-Until { $h = Get-Json '/api/health'; $h -and $h.status -eq 'ok' } $firstWait 'the recorder to answer'
            Wait-Until { $c = Get-Json '/api/card'; $c -and $c.status -eq 'ok' } $firstWait 'a first kept sample'
        } catch {
            return [ordered]@{ account = $Account; missing = @($required); unmet = @($wanted); failing = @("no answer: $($_.Exception.Message)"); sources = $null }
        }
        Start-Sleep -Seconds ([Math]::Max(21, 2 * $interval + 1))   # two more samples: CPU power is a rate between two readings
        $keys = Get-Keys
        $card = Get-Json '/api/card'
        $sources = if ($card -and $card.PSObject.Properties['sources']) { $card.sources } else { $null }
        $failing = @('counters', 'disk', 'gpu' | Where-Object { -not $sources -or ($sources.PSObject.Properties[$_] -and $sources.$_ -eq 'failing') })
        return [ordered]@{ account = $Account; missing = @($required | Where-Object { $_ -notin $keys }); unmet = @($wanted | Where-Object { $_ -notin $keys });
            failing = $failing; sources = $sources }
    }
    # Without anything to go by, a service already moved to LocalSystem stays there.
    $current = (Get-CimInstance Win32_Service -Filter "Name='$Service'").StartName
    # (Only a service that existed before this run: one created now starts as LocalSystem.)
    $accounts = if ($serviceExisted -and -not $required.Count -and -not $saved -and $current -eq 'LocalSystem') { @('LocalSystem') } else { @('NT AUTHORITY\LocalService', 'LocalSystem') }
    $checks = @()
    foreach ($a in $accounts) {
        $check = Test-Service $a
        $checks += $check
        if (-not ($check.missing -or $check.failing -or $check.unmet)) { break }
        if ($a -ne $accounts[-1]) { Warn "as ${a}: missing $(@($check.missing) + @($check.unmet) -join ', ') failing $($check.failing -join ', '); trying LocalSystem" }
    }
    # Of the accounts with every required reading, the one with the most probed ones (LocalService on
    # a tie); set up again if it is not the one tried last.
    $good = @($checks | Where-Object { -not ($_.missing -or $_.failing) } | Sort-Object -Stable { @($_.unmet).Count })
    if ($good -and $good[0].account -ne $checks[-1].account) { $check = Test-Service $good[0].account }
    elseif ($good) { $check = $checks[-1] }
    $status.account = $check.account
    $status.checks = $check
    if ($check.missing -or $check.failing) { throw "the recorder lacks readings it had before: missing $($check.missing -join ', '), failing $($check.failing -join ', ')" }
    if ($check.unmet) { Warn "not available under either account: $($check.unmet -join ', ')" }
    $readings = @(@($required) + @($wanted | Where-Object { $_ -notin $check.unmet }))
    Ok "running as $($check.account); readings: $($readings -join ', ')"
    # Kept for the next run, which then needs no recorder answering to know what to check.
    $state = [ordered]@{ account = $check.account; readings = $readings; at = (Get-Date).ToString('o') }
    [IO.File]::WriteAllText($StateFile, ($state | ConvertTo-Json), [Text.UTF8Encoding]::new($false))

    # Kymo records: the install under the earlier name goes, with the files of its own. Once its
    # service is gone there is no going back to it, so a failure in the cleanup is only a warning.
    if (Get-Service $OldService -ErrorAction SilentlyContinue) {
        Invoke-Native sc.exe @('delete', $OldService) -OkCodes 0, 1060, 1072 | Out-Null
        Ok "service $OldService removed"
    }
    $tookOver = $true
    Remove-Item -Force -LiteralPath $Uncommitted -ErrorAction SilentlyContinue
    try {
        foreach ($f in "$OldProgDir\perfhistory.exe", "$OldProgDir\perfhistory.exe.new") { Remove-Item -Force -LiteralPath $f -ErrorAction SilentlyContinue }
        # Its history goes only when this run copied it after stopping it; otherwise it may hold
        # samples Kymo's history lacks, so it stays where it is.
        if ($migratedNow) {
            Remove-Item -Recurse -Force -LiteralPath $OldHistory
            $status.migrated += ' (its old copy removed)'
        } elseif (Test-Path -LiteralPath $OldHistory) {
            Warn "the earlier install's history stays in $OldHistory (it was not copied by this run, so it may hold samples Kymo's history lacks); delete it when it is no longer needed"
        }
        Remove-Item -Force -LiteralPath "$OldPdRoot\install-state.json" -ErrorAction SilentlyContinue
        # The backups its imports left next to its history come along.
        Get-ChildItem -LiteralPath $OldPdRoot -Directory -Filter 'history.before-import-*' -ErrorAction SilentlyContinue |
            ForEach-Object { if (-not (Test-Path -LiteralPath "$PdRoot\$($_.Name)")) { Move-Item -LiteralPath $_.FullName -Destination $PdRoot } }
        Move-EarlierLogs
        # (The Grafana-based stack may still keep files there.)
        Remove-EmptyOldFolders
    } catch {
        Warn "the earlier install's service is removed, but not all of its files in $OldProgDir and $OldPdRoot (they can be deleted): $($_.Exception.Message)"
    }

    # ------------------------------------------------------------ old stack
    # The new service is up and checked: a failure from here on is a warning, never a failed install.
    if ($RemoveOldStack) { try {
        Step 'Previous stack'
        Stop-ScheduledTask -TaskPath '\PerfHistory\' -TaskName 'watchdog' -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskPath '\PerfHistory\' -TaskName 'watchdog' -Confirm:$false -ErrorAction SilentlyContinue
        try { $sched = New-Object -ComObject Schedule.Service; $sched.Connect(); $sched.GetFolder('\').DeleteFolder('PerfHistory', 0) } catch { }
        Ok 'watchdog task removed'
        if (Get-Service Grafana -ErrorAction SilentlyContinue) { Stop-Service Grafana -Force -ErrorAction SilentlyContinue }
        # A product counts as removed only once its service is gone too. One that is not keeps its
        # folder and the files it reads (a service left without them would fail at every boot); the
        # next run with -RemoveOldStack tries again.
        $grafanaGone = Remove-Product '^Grafana' 'Grafana'
        if ($grafanaGone) { Remove-Item -Recurse -Force 'C:\Program Files\GrafanaLabs' -ErrorAction SilentlyContinue }
        else { Warn 'Grafana kept, with its files: its uninstall did not finish' }
        foreach ($s in @(@('VictoriaMetrics', 'C:\Program Files\VictoriaMetrics\VictoriaMetrics.exe'), @('nvidia_gpu_exporter', 'C:\Program Files\nvidia_gpu_exporter\nvidia_gpu_exporter.exe'))) {
            if (Get-Service $s[0] -ErrorAction SilentlyContinue) {
                Stop-Service $s[0] -Force -ErrorAction SilentlyContinue
                if (Test-Path $s[1]) { & $s[1] uninstall 2>&1 | Out-Null }
                # 1060: already gone; 1072: marked for deletion (it goes once the last handle closes).
                if (Get-Service $s[0] -ErrorAction SilentlyContinue) { Invoke-Native sc.exe @('delete', $s[0]) -OkCodes 0, 1060, 1072 | Out-Null }
                Ok "$($s[0]) removed"
            }
            Remove-Item -Recurse -Force (Split-Path $s[1]) -ErrorAction SilentlyContinue
        }
        if (Remove-Product '^windows_exporter' 'windows_exporter') {
            Remove-Item -Recurse -Force 'C:\Program Files\windows_exporter' -ErrorAction SilentlyContinue
        } else { Warn 'windows_exporter kept, with its files: its uninstall did not finish' }
        # That stack's installer copies and watchdog, in the folders of the earlier name. Grafana's
        # secrets and dashboards only once Grafana is gone.
        $oldFiles = @("$OldProgDir\setup", "$OldProgDir\install.ps1", "$OldProgDir\uninstall.ps1", "$OldProgDir\verify.ps1", "$OldProgDir\test-resilience.ps1",
            "$OldProgDir\watchdog.ps1", "$OldProgDir\README.md", "$OldPdRoot\watchdog-state.json", "$OldPdRoot\install.lock")
        if ($grafanaGone) { $oldFiles += @("$OldProgDir\secrets", "$OldPdRoot\dashboards", (Join-Path $Here 'grafana-admin-password.txt')) }
        foreach ($p in $oldFiles) { Remove-Item -Recurse -Force -LiteralPath $p -ErrorAction SilentlyContinue }
        # Its watchdog is gone now, so the logs there can move too.
        Move-EarlierLogs
        Remove-EmptyOldFolders
        $left = @('Grafana', 'VictoriaMetrics', 'windows_exporter', 'nvidia_gpu_exporter' | Where-Object { Get-Service $_ -ErrorAction SilentlyContinue })
        $busy = @(9182, 9835, 8428, 21222 | Where-Object { Get-NetTCPConnection -State Listen -LocalPort $_ -ErrorAction SilentlyContinue })
        if ($left -or $busy) { Warn "still there: services $($left -join ', '); ports $($busy -join ', ')" }
        $status.old_stack = if ($left -or $busy) { 'partly removed' } else { 'removed (history kept in C:\ProgramData\VictoriaMetrics)' }
        Ok $status.old_stack
    } catch {
        $status.old_stack = 'partly removed'
        Warn "removing the previous stack stopped: $($_.Exception.Message); run install.ps1 -RemoveOldStack again"
    } }
    $h = Get-Json '/api/health'
    if (-not $h -or $h.status -ne 'ok') { throw 'the recorder stopped answering' }

    # ------------------------------------------------------------ test
    if ($Test) {
        Step 'Resilience test'
        & pwsh -NoProfile -ExecutionPolicy Bypass -File (Join-Path $Here 'test-resilience.ps1')
        $status.test = if ($LASTEXITCODE -eq 0) { 'passed' } else { "failed ($LASTEXITCODE)" }
        if ($LASTEXITCODE -ne 0) { Warn "resilience test failed ($LASTEXITCODE)" } else { Ok 'resilience test passed' }
    }
    $status.result = 'ok'
} catch {
    $status.error = ($_ | Out-String)
    Write-Host "`nFAILED: $($_.Exception.Message)" -ForegroundColor Red
} finally {
    # A first install of Kymo (or one a run cut short left before it took over) that failed while the
    # install under the earlier name can still run: back to that one, which then records again. The
    # Kymo service goes; the history copy made by this run is set aside (Kymo recorded into it during
    # the checks, while the earlier install was stopped), so a re-run copies the history again. Each
    # step is tried even when one before it failed. A Kymo install that had taken over is never
    # rolled back.
    $services = @($Service, 'windows_exporter', 'nvidia_gpu_exporter', 'VictoriaMetrics', 'Grafana')
    if ($status.result -ne 'ok' -and -not $tookOver -and $oldUsable -and (-not $serviceExisted -or $pendingAtStart)) {
        $services[0] = $OldService
        try {
            if (Get-Service $Service -ErrorAction SilentlyContinue) {
                Stop-Service $Service -Force -ErrorAction SilentlyContinue
                Invoke-Native sc.exe @('delete', $Service) -OkCodes 0, 1060, 1072 | Out-Null
            }
            Write-Host "  the Kymo service is removed" -ForegroundColor Yellow
        } catch { Warn "could not remove the Kymo service: $($_.Exception.Message)" }
        if ($migratedNow) {
            try {
                $aside = "$History.rolled-back-$stamp"
                Rename-Item -LiteralPath $History -NewName (Split-Path $aside -Leaf)
                Remove-Item -Force -LiteralPath $Uncommitted -ErrorAction SilentlyContinue
                Warn "the copy of the history this run made, with the recorder's log of the failed checks, is kept in $aside (it can be deleted)"
            } catch { Warn "the copy of the history could not be set aside (the next run copies it again): $($_.Exception.Message)" }
        }
        # The earlier service is started by the loop below, once it has settled after any stop.
        try { Wait-Until { (Get-Service $OldService).Status -in 'Stopped', 'Running' } 30 "the service $OldService to settle" } catch { }
    }
    # Never leave recording stopped: start the service (or, rolled back, the earlier one), or the old
    # stack's services if they are still there.
    foreach ($s in $services) {
        $x = Get-Service $s -ErrorAction SilentlyContinue
        if ($x -and $x.StartType -ne 'Disabled' -and $x.Status -ne 'Running') { try { Start-Service $s } catch { Write-Host "  could not start $s" -ForegroundColor Red } }
    }
    Write-Status
    Stop-Transcript | Out-Null
}
exit $(if ($status.result -eq 'ok') { 0 } else { 1 })
