# SPDX-License-Identifier: BSD-3-Clause

$ErrorActionPreference = "Stop"
$workspace = Resolve-Path (Join-Path $PSScriptRoot "../..")
$sako = Join-Path $workspace "target/release/sako.exe"
if (-not (Test-Path $sako)) { cargo build --manifest-path (Join-Path $workspace "Cargo.toml") --release -p sako-cli }
& $sako --memory-stats --detect-leaks (Join-Path $PSScriptRoot "workload.js")
if ($LASTEXITCODE -ne 0) { throw "memory benchmark failed" }
