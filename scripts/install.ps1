#Requires -RunAsAdministrator
<#
.SYNOPSIS
  Builds and registers the Rust VCam media source for the Windows Camera Frame Server.

.DESCRIPTION
  The Frame Server service runs as NT AUTHORITY\LocalService, which cannot read
  DLLs inside a user profile. This script copies the DLL to Program Files,
  registers its CLSID under HKLM with regsvr32, and creates a log folder the
  service can write to.

  This is for development. The installer (RustVCam-Setup-*.exe) registers the
  same camera from C:\Program Files\Rust VCam; whichever registered last is
  the DLL Windows loads. If the installed app's registration is active, this
  script stops unless -Force is given.

.PARAMETER Configuration
  Cargo profile to build and install: release (default) or debug.

.PARAMETER Force
  Register the development DLL even though the installed app's camera is
  registered. Rerun the installer afterwards to switch back.
#>
param(
    [ValidateSet('release', 'debug')]
    [string]$Configuration = 'release',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$installDir = Join-Path $env:ProgramFiles 'RustVCam'
$logDir = Join-Path $env:ProgramData 'RustVCam'
$dllName = 'rust_vcam_source.dll'

$clsidKey = 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Classes\CLSID\{E3C40D88-220A-4FD1-94D8-BC3F22833AFA}\InprocServer32'
$registered = (Get-ItemProperty -Path $clsidKey -ErrorAction SilentlyContinue).'(default)'
$installedDir = Join-Path $env:ProgramFiles 'Rust VCam'
if ($registered -and $registered.StartsWith($installedDir, [StringComparison]::OrdinalIgnoreCase) -and -not $Force) {
    throw "The installed Rust VCam ($registered) owns the camera registration. Re-run with -Force to register the development DLL instead (rerun the installer afterwards to switch back)."
}

$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }

Write-Host "Building vcam-source ($Configuration)..."
$buildArgs = @('build', '-p', 'vcam-source', '--manifest-path', (Join-Path $root 'Cargo.toml'))
if ($Configuration -eq 'release') { $buildArgs += '--release' }
& $cargo @buildArgs
if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }

# The Frame Server keeps the DLL loaded; stop it so the file can be replaced.
foreach ($service in 'FrameServerMonitor', 'FrameServer') {
    Stop-Service -Name $service -Force -ErrorAction SilentlyContinue
}

New-Item -ItemType Directory -Force -Path $installDir | Out-Null
$source = Join-Path $root "target\$Configuration\$dllName"
$target = Join-Path $installDir $dllName
Copy-Item -Path $source -Destination $target -Force
Write-Host "Copied $dllName to $installDir"

New-Item -ItemType Directory -Force -Path $logDir | Out-Null
& icacls.exe $logDir /grant '*S-1-5-19:(OI)(CI)M' | Out-Null   # S-1-5-19 = LOCAL SERVICE
Write-Host "Log folder: $logDir"

$proc = Start-Process -FilePath "$env:SystemRoot\System32\regsvr32.exe" -ArgumentList '/s', "`"$target`"" -Wait -PassThru
if ($proc.ExitCode -ne 0) { throw "regsvr32 failed with exit code $($proc.ExitCode)" }
Write-Host 'Registered CLSID {E3C40D88-220A-4FD1-94D8-BC3F22833AFA}.'
Write-Host 'Now run: cargo run -p vcam-app'
