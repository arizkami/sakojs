# SPDX-License-Identifier: BSD-3-Clause

param(
    [ValidateSet("startup", "filesystem", "package-cache")]
    [string]$Workload = "filesystem",
    [int]$Iterations = 10,
    [int]$SizeMiB = 16
)

$ErrorActionPreference = "Stop"
$workspace = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$wpr = Get-Command wpr.exe -ErrorAction Stop
$status = (& $wpr.Source -status 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) {
    throw "Unable to query WPR status: $status"
}
if ($status -notmatch "not recording") {
    throw "WPR is already recording; refusing to alter the active session."
}

$resultDirectory = Join-Path $workspace "benchmark-results"
New-Item -ItemType Directory -Force -Path $resultDirectory | Out-Null
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
$tracePath = Join-Path $resultDirectory "sako-$Workload-$stamp.etl"
$metadataPath = Join-Path $resultDirectory "sako-$Workload-$stamp.json"
$started = Get-Date
$recording = $false

try {
    & $wpr.Source -start GeneralProfile -filemode | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "WPR failed to start GeneralProfile."
    }
    $recording = $true

    switch ($Workload) {
        "startup" {
            & (Join-Path $workspace "benchmarks\startup\run.ps1") -Iterations $Iterations | Out-Host
        }
        "filesystem" {
            & (Join-Path $workspace "benchmarks\filesystem\run.ps1") -Iterations $Iterations -SizeMiB $SizeMiB | Out-Host
        }
        "package-cache" {
            & (Join-Path $workspace "benchmarks\package-cache\run.ps1") -Iterations $Iterations | Out-Host
        }
    }
    if ($LASTEXITCODE -ne 0) {
        throw "$Workload workload failed with exit code $LASTEXITCODE."
    }

    & $wpr.Source -stop $tracePath | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "WPR failed to save the trace."
    }
    $recording = $false
} finally {
    if ($recording) {
        & $wpr.Source -cancel | Out-Null
    }
}

$trace = Get-Item -LiteralPath $tracePath
$metadata = [ordered]@{
    workload = $Workload
    iterations = $Iterations
    size_mib = if ($Workload -eq "filesystem") { $SizeMiB } else { $null }
    started_utc = $started.ToUniversalTime().ToString("o")
    elapsed_ms = [math]::Round(((Get-Date) - $started).TotalMilliseconds, 3)
    trace_path = $trace.FullName
    trace_bytes = $trace.Length
    computer = $env:COMPUTERNAME
    os = (Get-CimInstance Win32_OperatingSystem).Caption
    cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
    rustc = (& rustc --version)
}
$metadata | ConvertTo-Json | Set-Content -LiteralPath $metadataPath -Encoding utf8
$metadata | ConvertTo-Json
