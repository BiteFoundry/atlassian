<#
.SYNOPSIS
    Idempotently installs `jira`, `confluence`, and `conf` terminal launchers on any Windows PC.

.DESCRIPTION
    1. Ensures <RepoRoot>\target\release\jira.exe exists (downloads the prebuilt
       release binary from GitHub `BiteFoundry/atlassian` via `gh` when missing, so
       Rust/Cargo is NOT required on target PCs).
    2. Registers/updates `# >>> atlassian launcher >>>` (`function jira`,
       `function confluence`, and `function conf`) in PowerShell $PROFILE. All three
       commands share the single `jira.exe` binary, with `confluence` and `conf`
       setting `ATLASSIAN_PERSONA=confluence` for the invocation.
    3. If $HOME\Tools\bin exists, also writes `jira.cmd`, `confluence.cmd`, and
       `conf.cmd` shims there so already-open shells find all three commands.

.EXAMPLE
    pwsh -File .\scripts\install-profile.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$ProfilePath = $PROFILE,
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ProfilePath)) {
    throw 'ProfilePath is empty. Pass -ProfilePath explicitly or run inside a PowerShell host where $PROFILE is defined.'
}

$resolvedRoot = if (Test-Path -LiteralPath $RepoRoot) {
    (Resolve-Path -LiteralPath $RepoRoot).Path
} else {
    $RepoRoot
}

$releaseDir = Join-Path $resolvedRoot 'target\release'
$releaseExe = Join-Path $releaseDir 'jira.exe'

if (-not (Test-Path -LiteralPath $releaseExe)) {
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh -and $PSCmdlet.ShouldProcess($releaseExe, 'Download prebuilt jira.exe from BiteFoundry/atlassian GitHub Release')) {
        Write-Output "[atlassian] Downloading prebuilt jira.exe from BiteFoundry/atlassian GitHub Release..."
        New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
        & $gh.Source release download -R 'BiteFoundry/atlassian' --pattern 'jira.exe' --dir $releaseDir --clobber
    }
}

$escapedRoot = $resolvedRoot.Replace("'", "''")
$startMarker = '# >>> atlassian launcher >>>'
$endMarker = '# <<< atlassian launcher <<<'

$block = @"
$startMarker
# Opens the unified Atlassian Jira + Confluence terminal client (CLI/TUI) from anywhere.
function jira {
    `$root = '$escapedRoot'
    `$exe = Join-Path `$root 'target\release\jira.exe'
    if (-not (Test-Path -LiteralPath `$exe)) {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            `$relDir = Split-Path -Parent `$exe
            New-Item -ItemType Directory -Force -Path `$relDir | Out-Null
            gh release download -R 'BiteFoundry/atlassian' --pattern 'jira.exe' --dir `$relDir --clobber 2>`$null
        }
    }
    if (-not (Test-Path -LiteralPath `$exe)) {
        cargo build --release --manifest-path (Join-Path `$root 'Cargo.toml')
        if (`$LASTEXITCODE -ne 0) { return }
    }
    & `$exe @args
}

# Opens the unified binary in Confluence persona.
function confluence {
    `$prevPersona = `$env:ATLASSIAN_PERSONA
    `$env:ATLASSIAN_PERSONA = 'confluence'
    try {
        jira @args
    } finally {
        if (`$null -eq `$prevPersona) {
            Remove-Item Env:ATLASSIAN_PERSONA -ErrorAction SilentlyContinue
        } else {
            `$env:ATLASSIAN_PERSONA = `$prevPersona
        }
    }
}

# Short alias for Confluence persona.
function conf {
    `$prevPersona = `$env:ATLASSIAN_PERSONA
    `$env:ATLASSIAN_PERSONA = 'confluence'
    try {
        jira @args
    } finally {
        if (`$null -eq `$prevPersona) {
            Remove-Item Env:ATLASSIAN_PERSONA -ErrorAction SilentlyContinue
        } else {
            `$env:ATLASSIAN_PERSONA = `$prevPersona
        }
    }
}
$endMarker
"@.TrimEnd("`r", "`n")

$parentDir = Split-Path -Parent $ProfilePath
if ($parentDir -and -not (Test-Path -LiteralPath $parentDir)) {
    if ($PSCmdlet.ShouldProcess($parentDir, 'Create PowerShell profile parent directory')) {
        New-Item -ItemType Directory -Force -Path $parentDir | Out-Null
    }
}

$existing = ''
if (Test-Path -LiteralPath $ProfilePath) {
    $existing = Get-Content -LiteralPath $ProfilePath -Raw -Encoding UTF8
    if ($null -eq $existing) { $existing = '' }
}

$pattern = '(?ms)^# >>> atlassian launcher >>>\r?\n.*?^# <<< atlassian launcher <<<\r?\n?'
if ([regex]::IsMatch($existing, $pattern)) {
    $updated = [regex]::Replace($existing, $pattern, { param($m) $block + [Environment]::NewLine }, 1)
    $action = 'Update atlassian launcher block'
} elseif ([string]::IsNullOrWhiteSpace($existing)) {
    $updated = $block + [Environment]::NewLine
    $action = 'Create profile with atlassian launcher block'
} else {
    $trimmed = $existing.TrimEnd("`r", "`n")
    $nl = [Environment]::NewLine
    $updated = $trimmed + $nl + $nl + $block + $nl
    $action = 'Append atlassian launcher block'
}

if ($updated -ne $existing) {
    if ($PSCmdlet.ShouldProcess($ProfilePath, $action)) {
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($ProfilePath, $updated, $utf8NoBom)
        Write-Output "$action in $ProfilePath."
    }
} else {
    Write-Output "atlassian launcher in $ProfilePath is already up to date."
}

$toolsBin = Join-Path $HOME 'Tools\bin'
if (Test-Path -LiteralPath $toolsBin) {
    foreach ($cmdName in @('jira.cmd', 'confluence.cmd', 'conf.cmd')) {
        $shimPath = Join-Path $toolsBin $cmdName
        $shimContent = "@echo off`r`nchcp 65001 >nul`r`n`"$resolvedRoot\$cmdName`" %*`r`n"
        if ($PSCmdlet.ShouldProcess($shimPath, "Write PATH shim $cmdName")) {
            [System.IO.File]::WriteAllText($shimPath, $shimContent, [System.Text.Encoding]::ASCII)
            Write-Output "Updated PATH shim: $shimPath"
        }
    }
}
