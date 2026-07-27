[CmdletBinding()]
param(
    [string]$Source,
    [string]$DestinationRoot,
    [ValidateSet('Copy', 'Junction')][string]$Mode = 'Copy',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
if (-not $Source) {
    $portableSource = Join-Path $PSScriptRoot 'skills\ynote-local'
    $Source = if (Test-Path -LiteralPath $portableSource) {
        $portableSource
    } else {
        Join-Path $repoRoot 'skills\ynote-local'
    }
}
$sourcePath = [IO.Path]::GetFullPath($Source)
if (-not (Test-Path -LiteralPath (Join-Path $sourcePath 'SKILL.md'))) {
    throw "SKILL.md not found under $sourcePath"
}

if (-not $DestinationRoot) {
    if ($env:CODEX_HOME) {
        $DestinationRoot = Join-Path $env:CODEX_HOME 'skills'
    } elseif (Test-Path -LiteralPath (Join-Path $env:USERPROFILE '.codex')) {
        $DestinationRoot = Join-Path $env:USERPROFILE '.codex\skills'
    } else {
        $DestinationRoot = Join-Path $env:USERPROFILE '.agents\skills'
    }
}
$destinationRootPath = [IO.Path]::GetFullPath($DestinationRoot)
$destination = [IO.Path]::GetFullPath((Join-Path $destinationRootPath 'ynote-local'))
$rootPrefix = $destinationRootPath.TrimEnd('\') + '\'
if (-not $destination.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase) -or
    [IO.Path]::GetFileName($destination) -ne 'ynote-local') {
    throw "Refusing unsafe destination: $destination"
}

New-Item -ItemType Directory -Path $destinationRootPath -Force | Out-Null
if (Test-Path -LiteralPath $destination) {
    if (-not $Force) {
        throw "Destination already exists: $destination. Pass -Force to replace this exact skill."
    }
    Remove-Item -LiteralPath $destination -Recurse -Force
}

if ($Mode -eq 'Junction') {
    New-Item -ItemType Junction -Path $destination -Target $sourcePath | Out-Null
} else {
    Copy-Item -LiteralPath $sourcePath -Destination $destination -Recurse
}

$binary = Join-Path $destination 'scripts\ynote-cli-0.4.1.exe'
if (-not (Test-Path -LiteralPath $binary)) {
    throw "Installed binary is missing: $binary"
}
$version = (& $binary --version | Out-String).Trim()
if ($LASTEXITCODE -ne 0) {
    throw 'Installed binary failed its version smoke test.'
}

[ordered]@{
    ok = $true
    skill = 'ynote-local'
    source = $sourcePath
    destination = $destination
    mode = $Mode
    executable = $binary
    version = $version
} | ConvertTo-Json -Depth 4
