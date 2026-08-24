# SPDX-License-Identifier: BSD-3-Clause
# Runs one HTTP throughput sample against a runtime, for quick iteration.
param(
    [string]$Executable = "target\release\sako.exe",
    [string[]]$Arguments = @("run", "benchmarks\comparison\http.mjs"),
    [int]$Port = 0,
    [string]$Duration = "8s",
    [int]$Connections = 100
)

$ErrorActionPreference = "Stop"
if ($Port -eq 0) {
    $Listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    $Port = ([Net.IPEndPoint]$Listener.LocalEndpoint).Port
    $Listener.Stop()
}
$Oha = ".deps\tools\comparison\oha.exe"
$All = @($Arguments + @([string]$Port))
$Server = Start-Process -FilePath $Executable -ArgumentList $All -PassThru -WindowStyle Hidden
try {
    $Deadline = [DateTime]::UtcNow.AddSeconds(15)
    $Ready = $false
    do {
        Start-Sleep -Milliseconds 50
        if ($Server.HasExited) { throw "server exited early" }
        try { $Ready = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/" -TimeoutSec 1).StatusCode -eq 200 } catch { $Ready = $false }
    } while (-not $Ready -and [DateTime]::UtcNow -lt $Deadline)
    if (-not $Ready) { throw "server did not become ready" }
    $Server.Refresh()
    $WorkingSet = $Server.WorkingSet64 / 1MB
    $Raw = Join-Path $env:TEMP "sako-bench-http.json"
    & $Oha -z $Duration -c $Connections --no-tui --no-color --output-format json -o $Raw "http://127.0.0.1:$Port/" | Out-Null
    $Result = Get-Content $Raw -Raw | ConvertFrom-Json
    $Server.Refresh()
    [pscustomobject]@{
        requests_per_second = [Math]::Round($Result.summary.requestsPerSec, 1)
        p95_latency_ms      = [Math]::Round($Result.latencyPercentiles.p95 * 1000, 3)
        idle_working_mib    = [Math]::Round($WorkingSet, 2)
        cpu_seconds         = [Math]::Round($Server.TotalProcessorTime.TotalSeconds, 2)
        errors              = ($Result.errorDistribution | ConvertTo-Json -Compress)
    }
} finally {
    if (-not $Server.HasExited) { Stop-Process -Id $Server.Id -Force }
    $Server.WaitForExit()
}
