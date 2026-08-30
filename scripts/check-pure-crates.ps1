$ErrorActionPreference = 'Stop'

$pureCrates = @('urouter-contracts')
$bannedPackages = @('axum', 'hyper', 'redis', 'reqwest', 'tokio', 'tower')

foreach ($crate in $pureCrates) {
    $tree = @(& cargo tree -p $crate --prefix none)
    if ($LASTEXITCODE -ne 0) {
        throw "cargo tree failed for $crate"
    }
    $packages = @($tree | ForEach-Object { ($_ -split ' ')[0] } | Sort-Object -Unique)
    $violations = @($packages | Where-Object { $bannedPackages -contains $_ })
    if ($violations.Count -gt 0) {
        throw "$crate depends on forbidden I/O packages: $($violations -join ', ')"
    }
}

Write-Output "Pure-crate dependency boundary passed for: $($pureCrates -join ', ')."
