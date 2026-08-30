param(
    [string]$OutputPath = "target/p2-chaos-report.json",
    [int]$RedisPort = 16380,
    [string]$Gateway,
    [int]$SoakRequests = 1000
)

$ErrorActionPreference = 'Stop'
$containerName = "urouter-p2-chaos-$PID"
$checks = [System.Collections.Generic.List[object]]::new()
$startedAt = [DateTimeOffset]::UtcNow

function Invoke-Gate {
    param([string]$Name, [string[]]$Arguments)
    $started = [System.Diagnostics.Stopwatch]::StartNew()
    & $Arguments[0] $Arguments[1..($Arguments.Count - 1)]
    $exitCode = $LASTEXITCODE
    $started.Stop()
    $checks.Add([ordered]@{
        name = $Name
        passed = ($exitCode -eq 0)
        exit_code = $exitCode
        elapsed_ms = $started.ElapsedMilliseconds
    })
}

try {
    docker run -d --rm --name $containerName -p "${RedisPort}:6379" redis:7-alpine | Out-Null
    $ready = $false
    foreach ($attempt in 1..30) {
        docker exec $containerName redis-cli ping 2>$null | Out-Null
        if ($LASTEXITCODE -eq 0) {
            $ready = $true
            break
        }
        Start-Sleep -Milliseconds 250
    }
    if (-not $ready) { throw 'Redis chaos container did not become ready' }

    $env:UROUTER_TEST_REDIS_URL = "redis://127.0.0.1:${RedisPort}/"
    Invoke-Gate 'typed_timeout_fallback' @('cargo', 'test', '-p', 'urouter-gateway', '--bin', 'urouter-gateway', 'timeout_uses_its_typed_fallback_chain')
    Invoke-Gate 'partial_stream_failure' @('cargo', 'test', '-p', 'urouter-gateway', '--bin', 'urouter-gateway', 'partial_stream_failure_never_runs_success_finalization')
    Invoke-Gate 'redis_library_contracts_before_restart' @('cargo', 'test', '-p', 'urouter-gateway', '--lib', '--', '--ignored')
    Invoke-Gate 'redis_gateway_contracts_before_restart' @('cargo', 'test', '-p', 'urouter-gateway', '--bin', 'urouter-gateway', '--', '--ignored')

    docker restart $containerName | Out-Null
    Invoke-Gate 'redis_library_contracts_after_restart' @('cargo', 'test', '-p', 'urouter-gateway', '--lib', '--', '--ignored')
    Invoke-Gate 'redis_gateway_contracts_after_restart' @('cargo', 'test', '-p', 'urouter-gateway', '--bin', 'urouter-gateway', '--', '--ignored')

    if ($Gateway) {
        Invoke-Gate 'gateway_slo' @(
            'cargo', 'run', '-q', '-p', 'urouter-soak', '--',
            '--gateway', $Gateway, '--requests', $SoakRequests.ToString(), '--json'
        )
    }
}
finally {
    docker rm -f $containerName 2>$null | Out-Null
    Remove-Item Env:UROUTER_TEST_REDIS_URL -ErrorAction SilentlyContinue
    $passed = -not ($checks | Where-Object { -not $_.passed })
    $report = [ordered]@{
        schema_version = 1
        started_at = $startedAt.ToString('O')
        completed_at = [DateTimeOffset]::UtcNow.ToString('O')
        redis_restart_count = 1
        passed = $passed
        checks = $checks
    }
    $resolvedOutput = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\$OutputPath"))
    New-Item -ItemType Directory -Force -Path ([System.IO.Path]::GetDirectoryName($resolvedOutput)) | Out-Null
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $resolvedOutput -Encoding utf8
    Write-Output "P2 chaos report: $resolvedOutput"
}

if (-not $passed) { exit 1 }
