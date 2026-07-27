[CmdletBinding()]
param(
    [string]$RepositoryRoot,
    [switch]$SkipBinaryContent
)

$ErrorActionPreference = 'Stop'
if (-not $RepositoryRoot) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$root = [IO.Path]::GetFullPath($RepositoryRoot)

$candidateLines = & git -C $root ls-files --cached --others --exclude-standard
if ($LASTEXITCODE -ne 0) {
    throw 'Unable to enumerate the public Git tree.'
}
$candidates = @($candidateLines | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })

$forbiddenFiles = @(
    '(?i)(^|/)\.ynote-manifest\.json$',
    '(?i)\.ynote\.json$',
    '(?i)\.sqlite(?:-wal|-shm)?$',
    '(?i)(^|/)runtime-config\.json$',
    '(?i)(^|/)setting\.json$',
    '(?i)(^|/)ynote-cli-daemon\.vbs$',
    '(?i)(^|/)id_(?:rsa|ed25519)[^/]*$',
    '(?i)\.(?:pem|pfx|key)$',
    '(?i)(^|/)(?:private|local-data|runtime-data|mirror|test-results)/'
)

$allowedMarkdown = @(
    'README.md',
    'skills/ynote-local/SKILL.md',
    'skills/ynote-local/references/cli-reference.md'
)

$violations = [Collections.Generic.List[string]]::new()
foreach ($relative in $candidates) {
    $normalized = $relative.Replace('\', '/')
    foreach ($pattern in $forbiddenFiles) {
        if ($normalized -match $pattern) {
            $violations.Add("forbidden runtime/private file: $normalized")
            break
        }
    }
    if ($normalized.EndsWith('.md', [StringComparison]::OrdinalIgnoreCase) -and
        $normalized -notin $allowedMarkdown) {
        $violations.Add("Markdown is not on the public documentation allowlist: $normalized")
    }
}

$textExtensions = @('.rs', '.toml', '.md', '.ps1', '.yml', '.yaml', '.html', '.json', '.txt')
$contentPatterns = [ordered]@{
    'absolute Windows user path' = '(?i)[A-Z]:\\Users\\'
    'Youdao desktop account identifier' = '(?i)\bweixin[a-z0-9]{16,}\b'
    'real Youdao note/resource identifier' = '\bWEB[0-9a-fA-F]{24,}\b'
    'private key material' = '-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----'
}

foreach ($relative in $candidates) {
    $normalized = $relative.Replace('\', '/')
    if ($normalized -eq 'scripts/check-public-tree.ps1') {
        continue
    }
    $extension = [IO.Path]::GetExtension($normalized)
    if ($extension -notin $textExtensions) {
        continue
    }
    $path = Join-Path $root $relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        continue
    }
    $content = [IO.File]::ReadAllText($path)
    foreach ($entry in $contentPatterns.GetEnumerator()) {
        if ($content -match $entry.Value) {
            $violations.Add("$($entry.Key): $normalized")
        }
    }
}

if (-not $SkipBinaryContent) {
    $binaryExtensions = @('.exe', '.dll')
    foreach ($relative in $candidates) {
        $normalized = $relative.Replace('\', '/')
        $extension = [IO.Path]::GetExtension($normalized)
        if ($extension -notin $binaryExtensions) {
            continue
        }
        $path = Join-Path $root $relative
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            continue
        }
        $content = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($path))
        foreach ($entry in $contentPatterns.GetEnumerator()) {
            if ($content -match $entry.Value) {
                $violations.Add("$($entry.Key) embedded in binary: $normalized")
            }
        }
        if ($content -match '(?i)[A-Z]:\\workspace\\') {
            $violations.Add("local workspace path embedded in binary: $normalized")
        }
    }
}

if ($violations.Count -gt 0) {
    $violations | Sort-Object -Unique | ForEach-Object { Write-Error $_ }
    throw "Public-tree privacy validation failed with $($violations.Count) finding(s)."
}

[ordered]@{
    ok = $true
    filesChecked = $candidates.Count
    markdownAllowlist = $allowedMarkdown
    message = 'No runtime mirror, user-specific path, account ID, note ID, or private key was found.'
} | ConvertTo-Json -Depth 4
