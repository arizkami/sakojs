# SPDX-License-Identifier: BSD-3-Clause

param(
    [Parameter(Mandatory = $true)]
    [string]$TracePath
)

$ErrorActionPreference = "Stop"
$trace = Get-Item -LiteralPath $TracePath
$xperf = Get-Command xperf.exe -ErrorAction Stop
$stem = Join-Path $trace.DirectoryName $trace.BaseName
$profilePath = "$stem-profile.csv"
$cpuDiskPath = "$stem-cpudisk.csv"
$diskIoPath = "$stem-diskio.csv"

& $xperf.Source -i $trace.FullName -o $profilePath -a profile -detail
if ($LASTEXITCODE -ne 0) { throw "xperf profile analysis failed." }
& $xperf.Source -i $trace.FullName -o $cpuDiskPath -a cpudisk -exes sako.exe
if ($LASTEXITCODE -ne 0) { throw "xperf CPU/disk analysis failed." }
& $xperf.Source -i $trace.FullName -o $diskIoPath -a diskio -summary
if ($LASTEXITCODE -ne 0) { throw "xperf disk I/O analysis failed." }

$processIds = [System.Collections.Generic.HashSet[int]]::new()
$moduleWeights = @{}
Select-String -LiteralPath $profilePath -Pattern "sako\.exe" | ForEach-Object {
    if ($_.Line -match '^\s*sako\.exe \(\s*(\d+)\),\s*(\d+),\s*[\d.]+,\s*(.+?)\s*$') {
        [void]$processIds.Add([int]$Matches[1])
        $module = $Matches[3]
        $weight = [long]$Matches[2]
        if (-not $moduleWeights.ContainsKey($module)) { $moduleWeights[$module] = 0L }
        $moduleWeights[$module] += $weight
    }
}
$modules = $moduleWeights.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object {
    [ordered]@{ module = $_.Key; sampled_weight = $_.Value }
}
[ordered]@{
    trace_path = $trace.FullName
    trace_bytes = $trace.Length
    profile_report = $profilePath
    cpu_disk_report = $cpuDiskPath
    disk_io_report = $diskIoPath
    sako_processes = $processIds.Count
    sako_modules = @($modules)
} | ConvertTo-Json -Depth 3
