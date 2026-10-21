#Requires -Version 7.2
<#
.SYNOPSIS
    Removes Kymo, the performance recorder: the service Kymo, its program
    C:\Program Files\Kymo\kymo.exe, and the desktop card and dashboards-at-sign-in tasks of your
    account. Run it elevated.
.PARAMETER DeleteHistory
    Also delete the recorded history (C:\ProgramData\Kymo\history), the installer's state and logs.
    Without it the history is kept, and a later install.ps1 continues recording into it.
#>
[CmdletBinding()]
param([switch]$DeleteHistory)
$ErrorActionPreference = 'Stop'
if (-not ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this script elevated.'
}
$Service = 'Kymo'
$ProgDir = 'C:\Program Files\Kymo'
$PdRoot = 'C:\ProgramData\Kymo'
if (Get-Service $Service -ErrorAction SilentlyContinue) {
    Stop-Service $Service -Force -ErrorAction SilentlyContinue
    & sc.exe delete $Service | Out-Null
    Write-Host "Service $Service removed."
}
Get-Process kymo -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$ProgDir\*" } | ForEach-Object { $_.Kill(); $_.WaitForExit(5000) | Out-Null }
Remove-Item -Recurse -Force -LiteralPath $ProgDir -ErrorAction SilentlyContinue
# The desktop card and the dashboards at sign-in.
try { & (Join-Path $PSScriptRoot 'desktop\install-desktop.ps1') -Remove } catch { Write-Warning "desktop tasks: $($_.Exception.Message)" }
if ($DeleteHistory) {
    # The history with the copies of it a migration or an import left next to it, the state and the logs.
    Remove-Item -Recurse -Force -LiteralPath $PdRoot -ErrorAction SilentlyContinue
    Write-Host 'History deleted.'
} else {
    Write-Host "History kept in $PdRoot\history (delete it with -DeleteHistory)."
}
Write-Host 'Done.'
