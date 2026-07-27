[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][ValidatePattern('^\d+\.\d+\.\d+$')][string]$Version,
    [string]$OutputDirectory,
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$skillSource = Join-Path $repoRoot 'skills\ynote-local'
& (Join-Path $PSScriptRoot 'check-public-tree.ps1') -RepositoryRoot $repoRoot -SkipBinaryContent
if ($LASTEXITCODE -ne 0) {
    throw 'public-tree privacy validation failed'
}
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repoRoot 'release'
}
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $outputPath -Force | Out-Null

if (-not $SkipBuild) {
    $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe'
    if (-not (Test-Path -LiteralPath $cargo)) {
        $cargo = (Get-Command cargo -ErrorAction Stop).Source
    }
    & $cargo test --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
    & $cargo clippy --all-targets --locked -- -D warnings
    if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }

    $previousEncodedRustFlags = [Environment]::GetEnvironmentVariable('CARGO_ENCODED_RUSTFLAGS', 'Process')
    $remapFlags = @(
        "--remap-path-prefix=$repoRoot=/source"
    )
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $remapFlags += "--remap-path-prefix=$env:USERPROFILE=/user"
    }
    $separator = [char]0x1f
    $encodedRustFlags = $remapFlags -join $separator
    if (-not [string]::IsNullOrWhiteSpace($previousEncodedRustFlags)) {
        $encodedRustFlags = $previousEncodedRustFlags + $separator + $encodedRustFlags
    }
    try {
        $env:CARGO_ENCODED_RUSTFLAGS = $encodedRustFlags
        & $cargo build --release --locked
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
    } finally {
        [Environment]::SetEnvironmentVariable(
            'CARGO_ENCODED_RUSTFLAGS',
            $previousEncodedRustFlags,
            'Process'
        )
    }
    Copy-Item -LiteralPath (Join-Path $repoRoot 'target\release\ynote-cli.exe') -Destination (Join-Path $skillSource "scripts\ynote-cli-$Version.exe") -Force
}

$binary = Join-Path $skillSource "scripts\ynote-cli-$Version.exe"
foreach ($required in @('SKILL.md', 'agents\openai.yaml', 'references\cli-reference.md', "scripts\ynote-cli-$Version.exe", 'scripts\std-3e8df03bf182ab6c.dll', 'scripts\libunwind.dll')) {
    if (-not (Test-Path -LiteralPath (Join-Path $skillSource $required))) {
        throw "Release input is missing: $required"
    }
}

$versionText = (& $binary --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0 -or $versionText -ne "ynote-cli $Version") {
    throw "Binary version mismatch: $versionText"
}
& (Join-Path $PSScriptRoot 'check-public-tree.ps1') -RepositoryRoot $repoRoot
if ($LASTEXITCODE -ne 0) {
    throw 'public binary privacy validation failed'
}

$stagingRoot = Join-Path $repoRoot ('.release-staging\' + [guid]::NewGuid().ToString('N'))
$portableRoot = Join-Path $stagingRoot "ynote-local-v$Version-windows"
$portableSkill = Join-Path $portableRoot 'skills\ynote-local'
$skillOnlyRoot = Join-Path $stagingRoot 'ynote-local'
New-Item -ItemType Directory -Path $portableSkill,$skillOnlyRoot -Force | Out-Null
try {
    Copy-Item -Path (Join-Path $skillSource '*') -Destination $portableSkill -Recurse -Force
    Copy-Item -Path (Join-Path $skillSource '*') -Destination $skillOnlyRoot -Recurse -Force
    foreach ($packagedSkill in @($portableSkill, $skillOnlyRoot)) {
        $packagedScripts = Join-Path $packagedSkill 'scripts'
        Get-ChildItem -LiteralPath $packagedScripts -Filter 'ynote-cli-*.exe' -File |
            Where-Object { $_.Name -ne "ynote-cli-$Version.exe" } |
            Remove-Item -Force
        $generatedLauncher = Join-Path $packagedScripts 'ynote-cli-daemon.vbs'
        if (Test-Path -LiteralPath $generatedLauncher) {
            Remove-Item -LiteralPath $generatedLauncher -Force
        }
    }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'install-skill.ps1') -Destination (Join-Path $portableRoot 'install.ps1')
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'setup-ynote.ps1') -Destination (Join-Path $portableRoot 'setup-ynote.ps1')
    Copy-Item -LiteralPath (Join-Path $repoRoot 'README.md') -Destination (Join-Path $portableRoot 'README.md')

    $portableZip = Join-Path $outputPath "ynote-local-v$Version-windows.zip"
    $skillZip = Join-Path $outputPath "ynote-local-skill-v$Version.zip"
    if (Test-Path -LiteralPath $portableZip) { Remove-Item -LiteralPath $portableZip -Force }
    if (Test-Path -LiteralPath $skillZip) { Remove-Item -LiteralPath $skillZip -Force }
    Compress-Archive -LiteralPath $portableRoot -DestinationPath $portableZip -CompressionLevel Optimal
    Compress-Archive -LiteralPath $skillOnlyRoot -DestinationPath $skillZip -CompressionLevel Optimal

    $hashRows = foreach ($path in @($portableZip, $skillZip)) {
        $item = Get-Item -LiteralPath $path
        '{0}  {1}' -f (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant(), $item.Name
    }
    $checksumPath = Join-Path $outputPath "SHA256SUMS-v$Version.txt"
    $hashRows | Set-Content -Encoding ASCII -LiteralPath $checksumPath

    [ordered]@{
        ok = $true
        version = $Version
        portableZip = $portableZip
        skillZip = $skillZip
        checksums = $checksumPath
        binary = $versionText
    } | ConvertTo-Json -Depth 4
} finally {
    if (Test-Path -LiteralPath $stagingRoot) {
        Remove-Item -LiteralPath $stagingRoot -Recurse -Force
    }
}
