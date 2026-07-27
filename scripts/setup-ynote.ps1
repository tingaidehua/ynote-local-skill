[CmdletBinding()]
param(
    [string]$SkillDirectory,
    [string]$MirrorDirectory,
    [ValidateRange(300, 86400)][int]$Interval = 900,
    [ValidateRange(0, 3600)][int]$Jitter = 120,
    [ValidateRange(1, 65535)][int]$Port = 4768,
    [switch]$Cloud,
    [switch]$InstallStartup,
    [switch]$Start
)

$ErrorActionPreference = 'Stop'
if (-not $MirrorDirectory) {
    $oneDriveRoot = @(
        $env:OneDrive,
        $env:OneDriveConsumer,
        $env:OneDriveCommercial
    ) | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_)
    } | Select-Object -First 1
    if (-not $oneDriveRoot) {
        $oneDriveRoot = Join-Path $env:USERPROFILE 'OneDrive'
    }
    New-Item -ItemType Directory -Path $oneDriveRoot -Force | Out-Null
    $MirrorDirectory = Join-Path $oneDriveRoot 'notes\YoudaoNote'
}
if (-not $SkillDirectory) {
    $root = if ($env:CODEX_HOME) {
        Join-Path $env:CODEX_HOME 'skills'
    } elseif (Test-Path -LiteralPath (Join-Path $env:USERPROFILE '.codex')) {
        Join-Path $env:USERPROFILE '.codex\skills'
    } else {
        Join-Path $env:USERPROFILE '.agents\skills'
    }
    $SkillDirectory = Join-Path $root 'ynote-local'
}
$skillPath = [IO.Path]::GetFullPath($SkillDirectory)
$mirrorPath = [IO.Path]::GetFullPath($MirrorDirectory)
$binary = Join-Path $skillPath 'scripts\ynote-cli-0.4.1.exe'
if (-not (Test-Path -LiteralPath $binary)) {
    throw "ynote-cli is not installed at $binary"
}

$doctorText = & $binary doctor --pretty | Out-String
if ($LASTEXITCODE -ne 0) {
    throw 'Youdao desktop discovery failed. Sign in to the Windows desktop client first.'
}
$doctor = $doctorText | ConvertFrom-Json
if (-not $doctor.ok) {
    throw 'Youdao desktop discovery returned an error.'
}

$refreshArguments = @('mirror', 'refresh', '--output', $mirrorPath, '--pretty')
if (-not $Cloud) {
    $refreshArguments += '--local-only'
}
$refreshText = & $binary @refreshArguments | Out-String
if ($LASTEXITCODE -ne 0) {
    throw 'Initial mirror refresh failed.'
}
$refresh = $refreshText | ConvertFrom-Json

if ($InstallStartup) {
    & $binary daemon install --output $mirrorPath --interval $Interval --jitter $Jitter --port $Port --pretty | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw 'Installing the current-user startup entry failed.'
    }
}

$startedPid = $null
if ($Start) {
    $arguments = @('daemon', 'run', '--output', $mirrorPath, '--interval', "$Interval", '--jitter', "$Jitter", '--port', "$Port")
    if (-not $Cloud) {
        $arguments += '--local-only'
    }
    $process = Start-Process -FilePath $binary -ArgumentList $arguments -WindowStyle Hidden -PassThru
    $startedPid = $process.Id
}

[ordered]@{
    ok = $true
    installedSkill = $skillPath
    mirror = $mirrorPath
    desktopAccount = $doctor.data.source.account
    notes = $refresh.data.notes
    resources = $refresh.data.resources
    cloudRequested = [bool]$Cloud
    startupInstalled = [bool]$InstallStartup
    startedPid = $startedPid
    web = if ($Start) { "http://127.0.0.1:$Port/" } else { $null }
} | ConvertTo-Json -Depth 5
