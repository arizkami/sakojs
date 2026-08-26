# SPDX-License-Identifier: BSD-3-Clause
#
# Install benchmarks for the package manager.
#
# Each fixture is measured four ways, because the four differ by more than an
# order of magnitude and quoting one number for "install time" hides which one
# was meant:
#
#   cold resolve  no metadata cache, no store, no lockfile   -- the worst case
#   cold install  no store, lockfile present                 -- a fresh clone
#   warm resolve  metadata cached, no lockfile               -- `sako update`
#   warm install  everything cached, lockfile present        -- the common case
#
# Cold is produced by pointing the store somewhere empty rather than by
# deleting the real one, so running this never costs the machine its cache.
#
#   .\benchmarks\package-install\run.ps1
#   .\benchmarks\package-install\run.ps1 -Fixtures tiny,medium -Runs 3

[CmdletBinding()]
param(
    [string[]] $Fixtures = @('tiny', 'medium', 'scoped', 'native', 'large'),
    [int] $Runs = 1,
    [string] $Sako = "$PSScriptRoot\..\..\target\release\sako.exe",
    [switch] $Perf
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $Sako)) {
    throw "no sako at $Sako -- run: cargo build --release"
}

# The graphs. Chosen for shape rather than size: a flat one, a deep one, one
# that is mostly scopes, and one that is mostly per-platform native packages
# the host must decline to download.
$Graphs = @{
    tiny   = '{"is-odd":"^3.0.1"}'
    medium = '{"chalk":"^4.1.2","commander":"^11.1.0","debug":"^4.3.4","semver":"^7.5.4","minimatch":"^9.0.3"}'
    scoped = '{"@changesets/cli":"^2.27.0"}'
    native = '{"esbuild":"^0.21.0","rollup":"^4.20.0"}'
    large  = '{"vite":"^5.4.0","@changesets/cli":"^2.27.0","typescript":"^5.5.0"}'
}

$Workspace = Join-Path ([System.IO.Path]::GetTempPath()) "sako-install-bench"
$Results = @()

function New-Fixture([string] $Name, [string] $Root) {
    if (-not $Graphs.ContainsKey($Name)) { throw "unknown fixture '$Name'" }
    New-Item -ItemType Directory -Force $Root | Out-Null
    $manifest = '{"name":"bench-' + $Name + '","version":"1.0.0","private":true,"dependencies":' + $Graphs[$Name] + '}'
    Set-Content -Path (Join-Path $Root 'package.json') -Value $manifest -Encoding utf8
}

# One measured install. `Store` is what LOCALAPPDATA points at, which is what
# decides cold from warm; the lockfile and node_modules are cleared here.
function Measure-Install {
    param(
        [string] $Root,
        [string] $Store,
        [switch] $KeepLock
    )
    Remove-Item -Recurse -Force (Join-Path $Root 'node_modules') -ErrorAction SilentlyContinue
    if (-not $KeepLock) {
        Remove-Item -Force (Join-Path $Root 'sako.lock') -ErrorAction SilentlyContinue
    }
    $previous = $env:LOCALAPPDATA
    $env:LOCALAPPDATA = $Store
    # Not a terminal here anyway, but explicit: the live view must never be
    # part of what is being timed.
    $env:SAKO_PROGRESS = '0'
    try {
        $arguments = @('install')
        if ($Perf) { $arguments += '--perf' }
        $output = & $Sako @arguments 2>&1
        $elapsed = (Measure-Command {
            Remove-Item -Recurse -Force (Join-Path $Root 'node_modules') -ErrorAction SilentlyContinue
            $output = & $Sako @arguments 2>&1
        }).TotalMilliseconds
    }
    finally {
        $env:LOCALAPPDATA = $previous
        Remove-Item Env:\SAKO_PROGRESS -ErrorAction SilentlyContinue
    }
    [pscustomobject]@{ Milliseconds = $elapsed; Output = $output }
}

foreach ($fixture in $Fixtures) {
    $root = Join-Path $Workspace $fixture
    $cold = Join-Path $Workspace "store-cold-$fixture"
    $warm = Join-Path $Workspace "store-warm-$fixture"
    Remove-Item -Recurse -Force $root, $cold, $warm -ErrorAction SilentlyContinue
    New-Fixture $fixture $root

    Write-Host "== $fixture ==" -ForegroundColor Cyan

    # Warm the shared store once, and keep the lockfile it writes.
    $env:LOCALAPPDATA = $warm
    $env:SAKO_PROGRESS = '0'
    & $Sako install --registry https://registry.npmjs.org 2>&1 | Out-Null
    Remove-Item Env:\SAKO_PROGRESS -ErrorAction SilentlyContinue
    $lock = Join-Path $root 'sako.lock'
    $lockCopy = Join-Path $Workspace "$fixture.lock"
    Copy-Item $lock $lockCopy -Force

    $measurements = [ordered]@{}
    foreach ($run in 1..$Runs) {
        # Cold resolve: nothing cached, no lockfile.
        Remove-Item -Recurse -Force $cold -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force $cold | Out-Null
        $value = (Measure-Install -Root $root -Store $cold).Milliseconds
        $measurements['cold resolve'] = [math]::Min($measurements['cold resolve'] ?? [double]::MaxValue, $value)

        # Cold install: nothing cached, lockfile present.
        Remove-Item -Recurse -Force $cold -ErrorAction SilentlyContinue
        New-Item -ItemType Directory -Force $cold | Out-Null
        Copy-Item $lockCopy $lock -Force
        $value = (Measure-Install -Root $root -Store $cold -KeepLock).Milliseconds
        $measurements['cold install'] = [math]::Min($measurements['cold install'] ?? [double]::MaxValue, $value)

        # Warm resolve: metadata and store cached, lockfile discarded.
        $value = (Measure-Install -Root $root -Store $warm).Milliseconds
        $measurements['warm resolve'] = [math]::Min($measurements['warm resolve'] ?? [double]::MaxValue, $value)

        # Warm install: everything cached, lockfile present.
        Copy-Item $lockCopy $lock -Force
        $value = (Measure-Install -Root $root -Store $warm -KeepLock).Milliseconds
        $measurements['warm install'] = [math]::Min($measurements['warm install'] ?? [double]::MaxValue, $value)
    }

    $packages = (Get-Content $lockCopy -Raw | ConvertFrom-Json).packages.PSObject.Properties.Count
    foreach ($key in $measurements.Keys) {
        Write-Host ("   {0,-14}{1,9:N0} ms" -f $key, $measurements[$key])
        $Results += [pscustomobject]@{
            Fixture      = $fixture
            Packages     = $packages
            Scenario     = $key
            Milliseconds = [math]::Round($measurements[$key])
        }
    }
    Write-Host ("   {0,-14}{1,9:N0} packages" -f 'graph', $packages) -ForegroundColor DarkGray
}

$report = Join-Path $PSScriptRoot 'results.json'
$Results | ConvertTo-Json -Depth 4 | Set-Content -Path $report -Encoding utf8
Write-Host "`nwrote $report" -ForegroundColor DarkGray
