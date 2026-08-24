# SPDX-License-Identifier: BSD-3-Clause

param([int]$SizeMiB = 16, [int]$Iterations = 100)
$ErrorActionPreference = "Stop"
if ($SizeMiB -lt 1 -or $SizeMiB -gt 1024 -or $Iterations -lt 1) { throw "invalid benchmark parameters" }

$workspace = Resolve-Path (Join-Path $PSScriptRoot "../..")
$sako = Join-Path $workspace "target/release/sako.exe"
if (-not (Test-Path $sako)) { cargo build --manifest-path (Join-Path $workspace "Cargo.toml") --release -p sako-cli }
$fixture = Join-Path $env:TEMP "sako-filesystem-$PID.bin"
try {
  $stream = [IO.File]::Open($fixture, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
  try {
    $stream.SetLength($SizeMiB * 1MB)
  } finally {
    $stream.Dispose()
  }
  & $sako (Join-Path $PSScriptRoot "workload.mjs") $fixture $Iterations
  if ($LASTEXITCODE -ne 0) { throw "filesystem benchmark failed" }
} finally {
  if (Test-Path -LiteralPath $fixture) { Remove-Item -LiteralPath $fixture -Force }
}
