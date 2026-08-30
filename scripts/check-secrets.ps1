$ErrorActionPreference = 'Stop'

$patterns = @(
    '(?<![A-Za-z0-9])sk-[A-Za-z0-9]{20,}',
    '(?<![A-Za-z0-9])gh[pousr]_[A-Za-z0-9]{30,}',
    'AKIA[0-9A-Z]{16}',
    '-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY-----'
)

$trackedFiles = @(& git -c core.quotePath=false ls-files --cached --others --exclude-standard)
if ($LASTEXITCODE -ne 0) {
    throw 'git ls-files failed'
}

$findings = @()
foreach ($relativePath in $trackedFiles) {
    $repositoryRoot = Join-Path -Path $PSScriptRoot -ChildPath '..'
    $fullPath = Join-Path -Path $repositoryRoot -ChildPath $relativePath
    if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
        continue
    }
    $lineNumber = 0
    try {
        foreach ($line in Get-Content -LiteralPath $fullPath -ErrorAction Stop) {
            $lineNumber++
            foreach ($pattern in $patterns) {
                if ($line -match $pattern) {
                    $findings += "${relativePath}:${lineNumber}"
                    break
                }
            }
        }
    }
    catch {
        # Binary and non-text tracked files are outside this lightweight P0 scanner.
        continue
    }
}

if ($findings.Count -gt 0) {
    Write-Error ("Potential secrets found at:`n" + ($findings -join "`n"))
    exit 1
}

Write-Output "Secret scan passed for $($trackedFiles.Count) repository files."
