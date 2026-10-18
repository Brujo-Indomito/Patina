#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Unregisters the Rust VCam media source and removes its installed files.

  This removes the development install made by install.ps1. If the installed
  app (C:\Program Files\Rust VCam) owns the camera registration, it is left
  alone, as is the shared log folder.

.PARAMETER KeepLogs
  Leave %ProgramData%\RustVCam (source.log) in place.
#>
param([switch]$KeepLogs)

$ErrorActionPreference = 'Stop'
$installDir = Join-Path $env:ProgramFiles 'RustVCam'
$logDir = Join-Path $env:ProgramData 'RustVCam'
$target = Join-Path $installDir 'rust_vcam_source.dll'
$clsidKey = 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Classes\CLSID\{E3C40D88-220A-4FD1-94D8-BC3F22833AFA}\InprocServer32'
$registered = (Get-ItemProperty -Path $clsidKey -ErrorAction SilentlyContinue).'(default)'
$installedApp = Test-Path (Join-Path $env:ProgramFiles 'Rust VCam\rust-vcam.exe')

foreach ($service in 'FrameServerMonitor', 'FrameServer') {
    Stop-Service -Name $service -Force -ErrorAction SilentlyContinue
}

if (Test-Path $target) {
    # Unregistering deletes the whole CLSID key, so only do it when it points
    # at the development DLL (not at the installed app).
    if ($registered -and [string]::Equals($registered, $target, [StringComparison]::OrdinalIgnoreCase)) {
        $proc = Start-Process -FilePath "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList '/u', '/s', "`"$target`"" -Wait -PassThru
        if ($proc.ExitCode -ne 0) { Write-Warning "regsvr32 /u exited with $($proc.ExitCode)" }
    } else {
        Write-Host "Camera registration points to $registered; leaving it."
    }
    Remove-Item -Recurse -Force $installDir
    Write-Host "Removed $installDir"
}

if (-not $KeepLogs -and -not $installedApp -and (Test-Path $logDir)) {
    Remove-Item -Recurse -Force $logDir
    Write-Host "Removed $logDir"
}
