#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Unregisters the Rust VCam media source and removes its installed files.

.PARAMETER KeepLogs
  Leave %ProgramData%\RustVCam (source.log) in place.
#>
param([switch]$KeepLogs)

$ErrorActionPreference = 'Stop'
$installDir = Join-Path $env:ProgramFiles 'RustVCam'
$logDir = Join-Path $env:ProgramData 'RustVCam'
$target = Join-Path $installDir 'rust_vcam_source.dll'

foreach ($service in 'FrameServerMonitor', 'FrameServer') {
    Stop-Service -Name $service -Force -ErrorAction SilentlyContinue
}

if (Test-Path $target) {
    $proc = Start-Process -FilePath "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList '/u', '/s', "`"$target`"" -Wait -PassThru
    if ($proc.ExitCode -ne 0) { Write-Warning "regsvr32 /u exited with $($proc.ExitCode)" }
    Remove-Item -Recurse -Force $installDir
    Write-Host "Removed $installDir"
}

if (-not $KeepLogs -and (Test-Path $logDir)) {
    Remove-Item -Recurse -Force $logDir
    Write-Host "Removed $logDir"
}
