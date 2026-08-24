# SPDX-License-Identifier: BSD-3-Clause

param(
    [ValidateRange(10, 10000)][int]$StartupIterations = 40,
    [ValidateRange(1, 100)][int]$WorkloadSamples = 5,
    [ValidateRange(1000000, 1000000000)][int]$ComputeIterations = 50000000,
    [ValidateRange(1, 1024)][int]$FileSizeMiB = 16,
    [ValidateRange(1, 10000)][int]$FileIterations = 30,
    [ValidateRange(1, 20)][int]$HttpSamples = 3,
    [ValidatePattern("^[1-9][0-9]*(ms|s|m)$")][string]$HttpDuration = "8s",
    [ValidateRange(1, 10000)][int]$HttpConnections = 100,
    [string]$ReportPath = "",
    [string]$DenoPath = "",
    [string]$OhaPath = "",
    [switch]$HighPerformance
)

$ErrorActionPreference = "Stop"
$Root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

# The Balanced power scheme parks this machine's cores below half their nominal
# clock and ramps them only after a workload has already been running, which
# moves short samples by up to 40% between runs. -HighPerformance pins the
# clock for the length of the run and restores the previous scheme afterwards.
$PreviousPowerScheme = $null
function Restore-PowerScheme {
    if ($null -ne $script:PreviousPowerScheme) {
        powercfg /setactive $script:PreviousPowerScheme | Out-Null
        Write-Host "Power scheme restored to $script:PreviousPowerScheme"
        $script:PreviousPowerScheme = $null
    }
}
trap { Restore-PowerScheme; break }
function Get-ProcessorPerformance {
    try {
        $Counter = Get-Counter "\Processor Information(_Total)\% Processor Performance" -ErrorAction Stop
        return [Math]::Round($Counter.CounterSamples[0].CookedValue, 1)
    } catch {
        return $null
    }
}
if ($HighPerformance) {
    $Active = ((powercfg /getactivescheme) -join " ")
    if ($Active -match "([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})") {
        $PreviousPowerScheme = $Matches[1]
        powercfg /setactive SCHEME_MIN | Out-Null
        Start-Sleep -Milliseconds 500
        Write-Host "Power scheme pinned to High performance (was $PreviousPowerScheme)"
    }
}
$Timestamp = [DateTime]::UtcNow.ToString("yyyyMMdd-HHmmss")
$ResultDirectory = Join-Path $Root "benchmark-results\comparison\$Timestamp"
New-Item -ItemType Directory -Force -Path $ResultDirectory | Out-Null

function Resolve-BenchmarkTool {
    param([string]$ExplicitPath, [string]$CommandName, [string]$VendoredPath)
    if (-not [string]::IsNullOrWhiteSpace($ExplicitPath)) {
        return [IO.Path]::GetFullPath($ExplicitPath)
    }
    $Command = Get-Command $CommandName -ErrorAction SilentlyContinue
    if ($null -ne $Command) { return $Command.Source }
    return $VendoredPath
}

$Sako = Join-Path $Root "target\release\sako.exe"
$Deno = Resolve-BenchmarkTool $DenoPath "deno" (Join-Path $Root ".deps\tools\comparison\deno.exe")
$Oha = Resolve-BenchmarkTool $OhaPath "oha" (Join-Path $Root ".deps\tools\comparison\oha.exe")
$Node = (Get-Command node -ErrorAction Stop).Source
$Bun = (Get-Command bun -ErrorAction Stop).Source
foreach ($Executable in @($Sako, $Deno, $Oha, $Node, $Bun)) {
    if (-not (Test-Path -LiteralPath $Executable)) {
        throw "benchmark executable is missing: $Executable"
    }
}

$Runtimes = @(
    [pscustomobject]@{ id = "sako"; name = "Sako.js"; executable = $Sako },
    [pscustomobject]@{ id = "node"; name = "Node.js"; executable = $Node },
    [pscustomobject]@{ id = "deno"; name = "Deno"; executable = $Deno },
    [pscustomobject]@{ id = "bun"; name = "Bun"; executable = $Bun }
)

function Get-RuntimeArguments {
    param($Runtime, [string]$Kind, [string]$Script, [string[]]$ScriptArguments = @())
    $Arguments = [System.Collections.Generic.List[string]]::new()
    if ($Runtime.id -eq "sako") {
        $Arguments.Add("run")
    } elseif ($Runtime.id -eq "deno") {
        $Arguments.Add("run")
        if ($Kind -eq "filesystem") { $Arguments.Add("--allow-read") }
        if ($Kind -eq "http") { $Arguments.Add("--allow-net") }
    }
    $Arguments.Add($Script)
    foreach ($Argument in $ScriptArguments) { $Arguments.Add([string]$Argument) }
    return $Arguments.ToArray()
}

function Invoke-Runtime {
    param($Runtime, [string]$Kind, [string]$Script, [string[]]$ScriptArguments = @())
    [string[]]$Arguments = @(Get-RuntimeArguments $Runtime $Kind $Script $ScriptArguments)
    $PreviousErrorActionPreference = $ErrorActionPreference
    try {
        $ErrorActionPreference = "Continue"
        $Output = & $Runtime.executable @Arguments 2>&1
        $ExitCode = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $PreviousErrorActionPreference
    }
    if ($ExitCode -ne 0) {
        throw "$($Runtime.name) failed for $Kind with exit code $ExitCode`n$($Output -join "`n")"
    }
    return ($Output -join "`n").Trim()
}

function Get-Percentile {
    param([double[]]$Values, [double]$Percentile)
    $Sorted = @($Values | Sort-Object)
    $Index = [Math]::Min($Sorted.Count - 1, [Math]::Floor($Sorted.Count * $Percentile))
    return [double]$Sorted[$Index]
}

function Get-Statistics {
    param([double[]]$Values)
    return [pscustomobject]@{
        samples = @($Values | ForEach-Object { [Math]::Round($_, 4) })
        median = [Math]::Round((Get-Percentile $Values 0.50), 4)
        p95 = [Math]::Round((Get-Percentile $Values 0.95), 4)
        minimum = [Math]::Round((($Values | Measure-Object -Minimum).Minimum), 4)
        maximum = [Math]::Round((($Values | Measure-Object -Maximum).Maximum), 4)
    }
}

function Measure-Startup {
    param($Runtime, [string]$Kind, [string]$Script)
    for ($Warmup = 0; $Warmup -lt 3; $Warmup++) {
        [void](Invoke-Runtime $Runtime $Kind $Script)
    }
    $Samples = [System.Collections.Generic.List[double]]::new()
    for ($Index = 0; $Index -lt $StartupIterations; $Index++) {
        $Timer = [System.Diagnostics.Stopwatch]::StartNew()
        [void](Invoke-Runtime $Runtime $Kind $Script)
        $Timer.Stop()
        $Samples.Add($Timer.Elapsed.TotalMilliseconds)
    }
    $Statistics = Get-Statistics $Samples.ToArray()
    return [pscustomobject]@{
        runtime = $Runtime.id
        median_ms = $Statistics.median
        p95_ms = $Statistics.p95
        minimum_ms = $Statistics.minimum
        maximum_ms = $Statistics.maximum
        samples_ms = $Statistics.samples
    }
}

function Measure-Compute {
    param($Runtime, [string]$Script)
    [void](Invoke-Runtime $Runtime "compute" $Script @([string]$ComputeIterations))
    $Rates = [System.Collections.Generic.List[double]]::new()
    $Checksum = $null
    for ($Index = 0; $Index -lt $WorkloadSamples; $Index++) {
        $Sample = (Invoke-Runtime $Runtime "compute" $Script @([string]$ComputeIterations)) | ConvertFrom-Json
        if ($null -eq $Checksum) { $Checksum = [uint32]$Sample.checksum }
        if ([uint32]$Sample.checksum -ne $Checksum) { throw "$($Runtime.name) compute checksum changed" }
        $Rates.Add($ComputeIterations / [double]$Sample.elapsedMilliseconds / 1000.0)
    }
    $Statistics = Get-Statistics $Rates.ToArray()
    return [pscustomobject]@{
        runtime = $Runtime.id
        median_million_ops_s = $Statistics.median
        p95_million_ops_s = $Statistics.p95
        checksum = $Checksum
        samples_million_ops_s = $Statistics.samples
    }
}

function Measure-Filesystem {
    param($Runtime, [string]$Script, [string]$Fixture)
    [void](Invoke-Runtime $Runtime "filesystem" $Script @($Fixture, [string]$FileIterations))
    $Rates = [System.Collections.Generic.List[double]]::new()
    for ($Index = 0; $Index -lt $WorkloadSamples; $Index++) {
        $Sample = (Invoke-Runtime $Runtime "filesystem" $Script @($Fixture, [string]$FileIterations)) | ConvertFrom-Json
        $Rates.Add(([double]$Sample.bytes / 1MB) / ([double]$Sample.elapsedMilliseconds / 1000.0))
    }
    $Statistics = Get-Statistics $Rates.ToArray()
    return [pscustomobject]@{
        runtime = $Runtime.id
        median_mib_s = $Statistics.median
        p95_mib_s = $Statistics.p95
        samples_mib_s = $Statistics.samples
    }
}

function Get-FreePort {
    $Listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    $Listener.Start()
    try { return ([Net.IPEndPoint]$Listener.LocalEndpoint).Port } finally { $Listener.Stop() }
}

function Sum-ObjectValues {
    param($Object)
    if ($null -eq $Object) { return 0 }
    $Total = 0
    foreach ($Property in $Object.PSObject.Properties) { $Total += [int64]$Property.Value }
    return $Total
}

function Measure-Http {
    param($Runtime, [string]$Script)
    $Rates = [System.Collections.Generic.List[double]]::new()
    $P95Latency = [System.Collections.Generic.List[double]]::new()
    $WorkingSets = [System.Collections.Generic.List[double]]::new()
    $PrivateBytes = [System.Collections.Generic.List[double]]::new()
    $Errors = 0
    $DeadlineAborts = 0
    for ($Index = 0; $Index -lt $HttpSamples; $Index++) {
        $Port = Get-FreePort
        [string[]]$Arguments = @(Get-RuntimeArguments $Runtime "http" $Script @([string]$Port))
        $Stdout = Join-Path $ResultDirectory "$($Runtime.id)-http-$Index.stdout.log"
        $Stderr = Join-Path $ResultDirectory "$($Runtime.id)-http-$Index.stderr.log"
        $Server = Start-Process -FilePath $Runtime.executable -ArgumentList $Arguments -PassThru -WindowStyle Hidden -RedirectStandardOutput $Stdout -RedirectStandardError $Stderr
        try {
            $Deadline = [DateTime]::UtcNow.AddSeconds(15)
            $Ready = $false
            do {
                Start-Sleep -Milliseconds 50
                if ($Server.HasExited) { break }
                try {
                    $Ready = (Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/" -TimeoutSec 1).StatusCode -eq 200
                } catch { $Ready = $false }
            } while (-not $Ready -and [DateTime]::UtcNow -lt $Deadline)
            if (-not $Ready) {
                $Failure = if (Test-Path $Stderr) { Get-Content $Stderr -Raw } else { "" }
                throw "$($Runtime.name) HTTP server did not become ready`n$Failure"
            }
            $Server.Refresh()
            $WorkingSets.Add($Server.WorkingSet64 / 1MB)
            $PrivateBytes.Add($Server.PrivateMemorySize64 / 1MB)
            $RawPath = Join-Path $ResultDirectory "$($Runtime.id)-oha-$Index.json"
            & $Oha -z $HttpDuration -c $HttpConnections --no-tui --no-color --output-format json -o $RawPath "http://127.0.0.1:$Port/"
            if ($LASTEXITCODE -ne 0) { throw "oha failed for $($Runtime.name)" }
            $Raw = Get-Content $RawPath -Raw | ConvertFrom-Json
            $Rates.Add([double]$Raw.summary.requestsPerSec)
            $P95Latency.Add([double]$Raw.latencyPercentiles.p95 * 1000.0)
            $SampleErrors = Sum-ObjectValues $Raw.errorDistribution
            $SampleDeadlineAborts = if ($null -ne $Raw.errorDistribution.'aborted due to deadline') {
                [int64]$Raw.errorDistribution.'aborted due to deadline'
            } else { 0 }
            $Errors += $SampleErrors - $SampleDeadlineAborts
            $DeadlineAborts += $SampleDeadlineAborts
        } finally {
            if (-not $Server.HasExited) { Stop-Process -Id $Server.Id -Force }
            $Server.WaitForExit()
        }
    }
    $RateStatistics = Get-Statistics $Rates.ToArray()
    $LatencyStatistics = Get-Statistics $P95Latency.ToArray()
    $WorkingSetStatistics = Get-Statistics $WorkingSets.ToArray()
    $PrivateStatistics = Get-Statistics $PrivateBytes.ToArray()
    return [pscustomobject]@{
        runtime = $Runtime.id
        median_requests_s = $RateStatistics.median
        p95_requests_s = $RateStatistics.p95
        median_p95_latency_ms = $LatencyStatistics.median
        idle_working_set_mib = $WorkingSetStatistics.median
        idle_private_mib = $PrivateStatistics.median
        errors = $Errors
        deadline_aborts = $DeadlineAborts
        samples_requests_s = $RateStatistics.samples
    }
}

function Invoke-VersionText {
    param([string]$Executable, [string[]]$Arguments)
    return ((& $Executable @Arguments 2>&1) -join " ").Trim()
}

if (-not (Test-Path -LiteralPath $Sako)) {
    throw "release Sako executable is missing; run cargo build --release -p sako-cli"
}

$HelloScript = Join-Path $PSScriptRoot "hello.js"
$TypeScript = Join-Path $PSScriptRoot "typescript.ts"
$ComputeScript = Join-Path $PSScriptRoot "compute.mjs"
$FilesystemScript = Join-Path $PSScriptRoot "filesystem.mjs"
$HttpScript = Join-Path $PSScriptRoot "http.mjs"
$FileFixture = Join-Path $ResultDirectory "filesystem-fixture.bin"
$Stream = [IO.File]::Open($FileFixture, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
try { $Stream.SetLength($FileSizeMiB * 1MB) } finally { $Stream.Dispose() }

$Startup = @()
$TypeScriptStartup = @()
$Compute = @()
$Filesystem = @()
$Http = @()
try {
    foreach ($Runtime in $Runtimes) {
        Write-Host "Benchmarking $($Runtime.name) startup"
        $Startup += Measure-Startup $Runtime "startup" $HelloScript
        Write-Host "Benchmarking $($Runtime.name) TypeScript startup"
        $TypeScriptStartup += Measure-Startup $Runtime "typescript" $TypeScript
        Write-Host "Benchmarking $($Runtime.name) compute"
        $Compute += Measure-Compute $Runtime $ComputeScript
        Write-Host "Benchmarking $($Runtime.name) filesystem"
        $Filesystem += Measure-Filesystem $Runtime $FilesystemScript $FileFixture
        Write-Host "Benchmarking $($Runtime.name) HTTP"
        $Http += Measure-Http $Runtime $HttpScript
    }
} catch {
    Restore-PowerScheme
    throw
} finally {
    if (Test-Path -LiteralPath $FileFixture) { Remove-Item -LiteralPath $FileFixture -Force }
}
$ProcessorPerformance = Get-ProcessorPerformance
Restore-PowerScheme

$Os = Get-CimInstance Win32_OperatingSystem
$Cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
$Commit = (git -C $Root rev-parse HEAD).Trim()
$Dirty = -not [string]::IsNullOrWhiteSpace((git -C $Root status --porcelain) -join "`n")
$PowerPlan = ((powercfg /getactivescheme 2>&1) -join " ").Trim()
$RuntimeInfo = @(
    [pscustomobject]@{ id = "sako"; name = "Sako.js"; version = Invoke-VersionText $Sako @("--version"); engine = "V8 14.9.0.0"; executable = $Sako },
    [pscustomobject]@{ id = "node"; name = "Node.js"; version = Invoke-VersionText $Node @("--version"); engine = (node -p "'V8 ' + process.versions.v8"); executable = $Node },
    [pscustomobject]@{ id = "deno"; name = "Deno"; version = ((Invoke-VersionText $Deno @("--version")) -split " ")[0..1] -join " "; engine = (& $Deno eval "console.log('V8 ' + Deno.version.v8)").Trim(); executable = $Deno },
    [pscustomobject]@{ id = "bun"; name = "Bun"; version = "bun $(Invoke-VersionText $Bun @("--version"))"; engine = "JavaScriptCore"; executable = $Bun }
)

$Result = [ordered]@{
    schema_version = 1
    generated_at_utc = [DateTime]::UtcNow.ToString("o")
    repository = [ordered]@{ commit = $Commit; dirty = $Dirty }
    environment = [ordered]@{
        os = $Os.Caption
        os_version = $Os.Version
        os_build = $Os.BuildNumber
        cpu = $Cpu.Name.Trim()
        physical_cores = $Cpu.NumberOfCores
        logical_processors = $Cpu.NumberOfLogicalProcessors
        memory_bytes = [int64]$Os.TotalVisibleMemorySize * 1KB
        power_plan = $PowerPlan
        power_scheme_pinned = [bool]$HighPerformance
        processor_performance_percent = $ProcessorPerformance
        rustc = (rustc --version)
        oha = Invoke-VersionText $Oha @("--version")
    }
    configuration = [ordered]@{
        startup_iterations = $StartupIterations
        startup_warmups = 3
        workload_samples = $WorkloadSamples
        compute_iterations = $ComputeIterations
        filesystem_size_mib = $FileSizeMiB
        filesystem_iterations = $FileIterations
        filesystem_cache = "warm operating-system cache"
        http_samples = $HttpSamples
        http_duration = $HttpDuration
        http_connections = $HttpConnections
        http_protocol = "HTTP/1.1 keep-alive"
        http_body = "Hello World"
        http_api = "node:http"
    }
    runtimes = $RuntimeInfo
    metrics = [ordered]@{
        javascript_startup = $Startup
        typescript_startup = $TypeScriptStartup
        integer_compute = $Compute
        filesystem_hot_read = $Filesystem
        http_plaintext = $Http
    }
}

$Json = $Result | ConvertTo-Json -Depth 12
$RawResult = Join-Path $ResultDirectory "results.json"
[IO.File]::WriteAllText($RawResult, $Json, [Text.UTF8Encoding]::new($false))
if ([string]::IsNullOrWhiteSpace($ReportPath)) {
    $ReportPath = Join-Path $Root "reports\runtime-comparison-$([DateTime]::UtcNow.ToString("yyyy-MM-dd")).html"
}
$ReportPath = [IO.Path]::GetFullPath($ReportPath)
New-Item -ItemType Directory -Force -Path (Split-Path $ReportPath) | Out-Null
$PublishedJson = [IO.Path]::ChangeExtension($ReportPath, ".json")
[IO.File]::WriteAllText($PublishedJson, $Json, [Text.UTF8Encoding]::new($false))
$Template = Get-Content (Join-Path $PSScriptRoot "report-template.html") -Raw
$Data = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($Json))
$Html = $Template.Replace("__BENCHMARK_DATA_BASE64__", $Data)
[IO.File]::WriteAllText($ReportPath, $Html, [Text.UTF8Encoding]::new($false))
Write-Host "Raw results: $RawResult"
Write-Host "Published JSON: $PublishedJson"
Write-Host "HTML report: $ReportPath"
