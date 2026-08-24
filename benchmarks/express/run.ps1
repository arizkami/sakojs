# SPDX-License-Identifier: BSD-3-Clause

param([string]$Duration = "10s", [int]$Connections = 100)
$ErrorActionPreference = "Stop"
if (-not (Get-Command oha -ErrorAction SilentlyContinue)) { throw "oha must be installed and on PATH" }
$workspace = Resolve-Path (Join-Path $PSScriptRoot "../..")
$sako = Join-Path $workspace "target/release/sako.exe"
if (-not (Test-Path $sako)) { cargo build --manifest-path (Join-Path $workspace "Cargo.toml") --release -p sako-cli }
if (-not (Test-Path (Join-Path $PSScriptRoot "node_modules/express"))) {
  Push-Location $PSScriptRoot
  try { & $sako install --ignore-scripts; if ($LASTEXITCODE -ne 0) { throw "Express install failed" } } finally { Pop-Location }
}
$server = Start-Process -FilePath $sako -ArgumentList @((Join-Path $PSScriptRoot "app.js")) -WorkingDirectory $PSScriptRoot -PassThru -WindowStyle Hidden
try {
  $deadline = [DateTime]::UtcNow.AddSeconds(10)
  do {
    Start-Sleep -Milliseconds 50
    try { $ready = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:3000/" -TimeoutSec 1).StatusCode -eq 200 } catch { $ready = $false }
  } while (-not $ready -and [DateTime]::UtcNow -lt $deadline)
  if (-not $ready) { throw "Express benchmark server did not become ready" }
  & oha -z $Duration -c $Connections --no-tui "http://127.0.0.1:3000/"
  if ($LASTEXITCODE -ne 0) { throw "oha failed" }
} finally {
  if (-not $server.HasExited) { Stop-Process -Id $server.Id -Force }
}
