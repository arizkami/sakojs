# SPDX-License-Identifier: BSD-3-Clause

param([int[]]$Workers = @(1, 2, 4), [int]$Samples = 5)
$ErrorActionPreference = "Stop"
$workspace = Resolve-Path (Join-Path $PSScriptRoot "../..")
$sako = Join-Path $workspace "target/release/sako.exe"
if (-not (Test-Path $sako)) { cargo build --manifest-path (Join-Path $workspace "Cargo.toml") --release -p sako-cli }
foreach ($count in $Workers) {
  if ($count -lt 1 -or $count -gt 256) { throw "worker count must be 1..256" }
  $values = for ($sample = 0; $sample -lt $Samples; $sample++) {
    (Measure-Command { & $sako "--workers=$count" (Join-Path $PSScriptRoot "workload.js") }).TotalMilliseconds
    if ($LASTEXITCODE -ne 0) { throw "worker benchmark failed" }
  }
  $ordered = @($values | Sort-Object)
  [pscustomobject]@{ benchmark = "worker_scaling"; workers = $count; samples = $Samples; median_ms = $ordered[[math]::Floor($ordered.Count / 2)] } | ConvertTo-Json -Compress
}
