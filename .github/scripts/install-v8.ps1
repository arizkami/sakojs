# SPDX-License-Identifier: BSD-3-Clause

param(
  [Parameter(Mandatory = $true)][string]$Url,
  [string]$Destination = ".deps/v8"
)

$ErrorActionPreference = "Stop"
$archive = Join-Path $env:RUNNER_TEMP "sako-v8.zip"
$staging = Join-Path $env:RUNNER_TEMP "sako-v8-extracted"
Invoke-WebRequest -Uri $Url -OutFile $archive
Expand-Archive -LiteralPath $archive -DestinationPath $staging -Force

$root = if (Test-Path (Join-Path $staging "include/v8.h")) {
  $staging
} elseif (Test-Path (Join-Path $staging "v8/include/v8.h")) {
  Join-Path $staging "v8"
} else {
  throw "V8 artifact must contain include/v8.h at its root or under v8/"
}

New-Item -ItemType Directory -Force -Path (Split-Path $Destination) | Out-Null
Copy-Item -LiteralPath $root -Destination $Destination -Recurse -Force

foreach ($required in @("include/v8.h", "lib/v8_monolith.lib", "bin/icudtl.dat")) {
  if (-not (Test-Path (Join-Path $Destination $required))) {
    throw "V8 artifact is missing $required"
  }
}
