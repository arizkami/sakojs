# SPDX-License-Identifier: BSD-3-Clause

param(
    [ValidateRange(10, 100000)]
    [int]$Iterations = 100
)

$ErrorActionPreference = "Stop"
$Root = Resolve-Path (Join-Path $PSScriptRoot "..\..")
$Executable = Join-Path $Root "target\release\sako.exe"
$Fixture = Join-Path $Root "tests\hello.js"

if (-not (Test-Path -LiteralPath $Executable)) {
    throw "Build the release executable first: cargo build --release -p sako-cli"
}

& $Executable $Fixture *> $null
$Samples = [System.Collections.Generic.List[double]]::new()
for ($Index = 0; $Index -lt $Iterations; $Index++) {
    $Timer = [System.Diagnostics.Stopwatch]::StartNew()
    & $Executable $Fixture *> $null
    if ($LASTEXITCODE -ne 0) { throw "Sako exited with $LASTEXITCODE" }
    $Timer.Stop()
    $Samples.Add($Timer.Elapsed.TotalMilliseconds)
}
$Sorted = $Samples | Sort-Object
$Median = $Sorted[[Math]::Floor($Sorted.Count * 0.50)]
$P95 = $Sorted[[Math]::Min($Sorted.Count - 1, [Math]::Floor($Sorted.Count * 0.95))]
[pscustomobject]@{
    benchmark = "startup_hello"
    iterations = $Iterations
    median_ms = [Math]::Round($Median, 3)
    p95_ms = [Math]::Round($P95, 3)
} | ConvertTo-Json -Compress
