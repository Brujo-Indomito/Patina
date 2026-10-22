<#
.SYNOPSIS
  Runs a command, waits for it to settle, and reports its memory and CPU use.

.DESCRIPTION
  Reports working set, private bytes (memory that belongs only to the
  process), GPU dedicated/shared memory (from the "GPU Process Memory"
  performance counters), and CPU over a sampling window.

.EXAMPLE
  .\scripts\measure-memory.ps1 -Exe target\release\patina-cli.exe -Arguments '--bench-effects','--seconds','25'
#>
param(
    [Parameter(Mandatory)] [string]$Exe,
    [string[]]$Arguments = @(),
    [int]$SettleSeconds = 12,
    [int]$SampleSeconds = 6,
    [string]$Label = ''
)

$ErrorActionPreference = 'Stop'
$log = Join-Path $env:TEMP "measure-memory-$PID.txt"
$process = Start-Process -FilePath $Exe -ArgumentList $Arguments -NoNewWindow -PassThru -RedirectStandardOutput $log
try {
    Start-Sleep -Seconds $SettleSeconds
    $process.Refresh()
    if ($process.HasExited) { throw "process exited early; output:`n$(Get-Content $log -Raw)" }

    $cpuStart = $process.TotalProcessorTime
    $timeStart = Get-Date
    Start-Sleep -Seconds $SampleSeconds
    $process.Refresh()
    $cpu = ($process.TotalProcessorTime - $cpuStart).TotalSeconds / ((Get-Date) - $timeStart).TotalSeconds

    $gpuDedicated = 0; $gpuShared = 0
    try {
        $instances = (Get-Counter -ListSet 'GPU Process Memory').PathsWithInstances |
            Where-Object { $_ -like "*pid_$($process.Id)_*" }
        if ($instances) {
            $samples = (Get-Counter -Counter $instances -ErrorAction SilentlyContinue).CounterSamples
            $gpuDedicated = ($samples | Where-Object { $_.Path -like '*dedicated usage' } | Measure-Object CookedValue -Sum).Sum
            $gpuShared = ($samples | Where-Object { $_.Path -like '*shared usage' } | Measure-Object CookedValue -Sum).Sum
        }
    } catch { }

    [pscustomobject]@{
        Label          = $Label
        WorkingSetMB   = [math]::Round($process.WorkingSet64 / 1MB)
        PrivateMB      = [math]::Round($process.PrivateMemorySize64 / 1MB)
        GpuDedicatedMB = [math]::Round($gpuDedicated / 1MB)
        GpuSharedMB    = [math]::Round($gpuShared / 1MB)
        CpuPercentCore = [math]::Round($cpu * 100)
    }
    $last = Get-Content $log | Where-Object { $_ -match 'process\(\)|Segmentation|Webcam delivering' } | Select-Object -Last 1
    if ($last) { Write-Host "  $last" }
} finally {
    if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force }
    Remove-Item $log -ErrorAction SilentlyContinue
}
