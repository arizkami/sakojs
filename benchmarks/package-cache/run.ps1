# SPDX-License-Identifier: BSD-3-Clause

param([int]$Iterations = 5)
$ErrorActionPreference = "Stop"
if ($Iterations -lt 1 -or $Iterations -gt 100) { throw "iterations must be 1..100" }
$workspace = Resolve-Path (Join-Path $PSScriptRoot "../..")
$benchmarks = Resolve-Path (Join-Path $workspace "benchmarks")
$project = Resolve-Path (Join-Path $benchmarks "express")
if (-not $project.Path.StartsWith($benchmarks.Path, [StringComparison]::OrdinalIgnoreCase)) { throw "project escaped benchmark root" }
$sako = Join-Path $workspace "target/release/sako.exe"
if (-not (Test-Path $sako)) { cargo build --manifest-path (Join-Path $workspace "Cargo.toml") --release -p sako-cli }

$samples = @()
Push-Location $project
try {
  for ($iteration = 0; $iteration -lt $Iterations; $iteration++) {
    $modules = Join-Path $project "node_modules"
    if (Test-Path -LiteralPath $modules) { Remove-Item -LiteralPath $modules -Recurse -Force }
    $samples += (Measure-Command { & $sako install --ignore-scripts }).TotalMilliseconds
    if ($LASTEXITCODE -ne 0) { throw "cached install benchmark failed" }
  }
} finally {
  Pop-Location
}
$ordered = @($samples | Sort-Object)
[pscustomobject]@{ benchmark = "locked_cached_install"; iterations = $Iterations; median_ms = $ordered[[math]::Floor($ordered.Count / 2)]; samples_ms = $samples } | ConvertTo-Json -Compress
