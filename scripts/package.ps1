<#
.SYNOPSIS
  Builds Patina in release mode and produces target\installer\Patina-Setup-<version>.exe.

.DESCRIPTION
  Stages the executables, the camera DLL, DirectML.dll, the segmentation
  models found in models\, and the Visual C++ runtime DLLs (app-local
  deployment) into target\package, then compiles installer\patina.iss with
  Inno Setup 6 (ISCC.exe).

  Patina must not be running: the build replaces its executables.
#>
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$release = Join-Path $root 'target\release'
$package = Join-Path $root 'target\package'

$running = Get-Process patina, patina-cli -ErrorAction SilentlyContinue
if ($running) { throw "Quit Patina first (running: $($running.Name -join ', '))." }

$cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
if (-not (Test-Path $cargo)) { $cargo = 'cargo' }
# Native tools write progress and warnings to stderr; judge them by exit code
# (Windows PowerShell 5.1 would otherwise treat stderr output as an error).
function Invoke-Native([scriptblock]$Command, [string]$Name) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try { & $Command 2>&1 | ForEach-Object { "$_" } } finally { $ErrorActionPreference = $previous }
    if ($LASTEXITCODE -ne 0) { throw "$Name failed ($LASTEXITCODE)" }
}

if (-not $SkipBuild) {
    Invoke-Native { & $cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') } 'cargo build'
}

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value

Remove-Item $package -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force (Join-Path $package 'models') | Out-Null

foreach ($file in 'patina.exe', 'patina-cli.exe', 'patina_source.dll', 'DirectML.dll') {
    Copy-Item (Join-Path $release $file) $package
}

# Only the Apache-2.0 MediaPipe model is bundled, so the installer carries no
# GPL code; RVM stays an optional download (see README).
$model = Join-Path $root 'models\selfie_segmenter_landscape.onnx'
if (-not (Test-Path $model)) { throw 'models\selfie_segmenter_landscape.onnx not found (see README).' }
Copy-Item $model (Join-Path $package 'models')

# Visual C++ runtime, deployed next to the app (allowed by the redistributable license).
$crt = Get-ChildItem "${env:ProgramFiles(x86)}\Microsoft Visual Studio\*\*\VC\Redist\MSVC\*\x64\Microsoft.VC14*.CRT" -Directory |
    Sort-Object FullName | Select-Object -Last 1
if (-not $crt) { throw 'Visual C++ redistributable DLLs not found (install the VS Build Tools C++ workload).' }
foreach ($dll in 'vcruntime140.dll', 'vcruntime140_1.dll', 'msvcp140.dll', 'msvcp140_1.dll', 'msvcp140_2.dll') {
    Copy-Item (Join-Path $crt.FullName $dll) $package
}

# The icon written by crates\patina-app\build.rs.
$icon = Get-ChildItem (Join-Path $release 'build\patina-app-*\out\patina.ico') | Sort-Object LastWriteTime | Select-Object -Last 1
Copy-Item $icon.FullName (Join-Path $package 'patina.ico')

$iscc = @("$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe", "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe") |
    Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (winget install JRSoftware.InnoSetup).' }
Invoke-Native { & $iscc "/DAppVersion=$version" /Q (Join-Path $root 'installer\patina.iss') } 'Inno Setup'

$setup = Join-Path $root "target\installer\Patina-Setup-$version.exe"
$size = [math]::Round((Get-Item $setup).Length / 1MB, 1)
Write-Host "Built $setup ($size MB)"
