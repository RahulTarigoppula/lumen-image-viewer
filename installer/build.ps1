<#
.SYNOPSIS
  Builds Lumen-Setup.exe from a built lumen.exe with Inno Setup 6.
.DESCRIPTION
  Finds ISCC.exe (installs Inno Setup with Chocolatey when it is missing or
  older than 6.3), reads the version from Cargo.toml ([workspace.package]) and
  compiles installer\lumen.iss. Used by .github/workflows/build.yml (release)
  and dev.yml (install test).
.EXAMPLE
  installer/build.ps1 -Exe dist/Lumen.exe -Out dist
#>
param(
    [Parameter(Mandatory)] [string] $Exe,
    [string] $Out = "dist",
    # Default: the first `version = "..."` in Cargo.toml (the workspace version).
    [string] $Version,
    # lzma2/max for releases; dev CI uses lzma2/fast on the large debug build.
    [string] $Compression = "lzma2/max"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

if (-not $Version) {
    $m = Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $m) { throw "No version found in Cargo.toml" }
    $Version = $m.Matches[0].Groups[1].Value
}
$exePath = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force $Out | Out-Null
$outPath = (Resolve-Path $Out).Path

function Find-Iscc {
    # The install folder first: the iscc.exe on PATH of the GitHub images is a
    # Chocolatey shim without version information.
    foreach ($base in @(${env:ProgramFiles(x86)}, $env:ProgramFiles, (Join-Path $env:LOCALAPPDATA "Programs"))) {
        if (-not $base) { continue }
        $p = Join-Path $base "Inno Setup 6\ISCC.exe"
        if (Test-Path $p) { return $p }
    }
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    return $null
}

function Get-IsccVersion([string] $path) {
    $v = (Get-Item $path).VersionInfo
    return [version]::new($v.FileMajorPart, $v.FileMinorPart, $v.FileBuildPart)
}

# x64compatible needs Inno Setup 6.3 or later (0.0.0: no version information, assume it is new enough).
$iscc = Find-Iscc
$old = $iscc -and (Get-IsccVersion $iscc) -gt [version]"0.0.0" -and (Get-IsccVersion $iscc) -lt [version]"6.3.0"
if (-not $iscc -or $old) {
    Write-Host "Inno Setup 6.3+ not found (have: $iscc), installing it with Chocolatey"
    choco upgrade innosetup -y --no-progress
    if ($LASTEXITCODE -ne 0) { throw "choco upgrade innosetup failed (exit code $LASTEXITCODE)" }
    $iscc = Find-Iscc
    if (-not $iscc) { throw "ISCC.exe not found after installing Inno Setup" }
}
Write-Host "Inno Setup $(Get-IsccVersion $iscc): $iscc"
Write-Host "Lumen $Version from $exePath, $Compression"

& $iscc "/DMyAppVersion=$Version" "/DMyAppExe=$exePath" "/DMyCompression=$Compression" "/O$outPath" (Join-Path $PSScriptRoot "lumen.iss")
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit code $LASTEXITCODE)" }

$setup = Join-Path $outPath "Lumen-Setup.exe"
if (-not (Test-Path $setup)) { throw "ISCC did not write $setup" }
Write-Host ("Built {0} ({1:N1} MB)" -f $setup, ((Get-Item $setup).Length / 1MB))
