# Builds KymoCard.exe (the card and the process list) with the C# compiler that ships with Windows (.NET Framework 4.8, C# 5).
# No SDK or download needed. Usage: powershell -File build.ps1 [-Out path]
# install-desktop.ps1 builds to KymoCard.exe.new first, so a running widget is replaced only after a good build.
param([string]$Out = (Join-Path $PSScriptRoot 'KymoCard.exe'))
$ErrorActionPreference = 'Stop'
$fw = "$env:SystemRoot\Microsoft.NET\Framework64\v4.0.30319"
$tmp = "$Out.tmp"
& "$fw\csc.exe" /nologo /target:winexe /optimize+ /platform:x64 /out:$tmp `
    /r:System.dll /r:System.Core.dll /r:System.Drawing.dll /r:System.Windows.Forms.dll /r:"$fw\System.Web.Extensions.dll" `
    (Join-Path $PSScriptRoot 'KymoCard.cs') (Join-Path $PSScriptRoot 'Processes.cs') (Join-Path $PSScriptRoot 'Actions.cs')
if ($LASTEXITCODE -ne 0) { throw "csc failed ($LASTEXITCODE)" }
Move-Item -Force $tmp $Out
Write-Output "built $Out ($((Get-Item $Out).Length) bytes)"
