<#
.SYNOPSIS
  Download Mesa's software OpenGL (llvmpipe) for Windows x64 into a folder.

.DESCRIPTION
  Fetches the pinned pal1000/mesa-dist-win release, checks its SHA-256 and
  copies the x64 OpenGL DLLs (opengl32.dll, libgallium_wgl.dll and, on older
  releases, libglapi.dll) into -Dest. Copy them next to lumen.exe and set
  GALLIUM_DRIVER=llvmpipe to run Lumen on a machine without a GPU (CI).
  Needs 7-Zip (7z.exe on PATH or in Program Files).
#>
param(
    [string]$Dest = "mesa",
    [string]$Version = "26.1.8",
    # SHA-256 of mesa3d-<Version>-release-msvc.7z; empty skips the check (prints the hash).
    [string]$Sha256 = "",
    [string]$Work = $env:RUNNER_TEMP
)
$ErrorActionPreference = "Stop"

if (-not $Work) { $Work = [System.IO.Path]::GetTempPath() }
$name = "mesa3d-$Version-release-msvc.7z"
$url = "https://github.com/pal1000/mesa-dist-win/releases/download/$Version/$name"
$dl = Join-Path $Work "mesa-dl-$Version"
New-Item -ItemType Directory -Force $dl | Out-Null
$archive = Join-Path $dl $name

if (-not (Test-Path $archive)) {
    Write-Host "Downloading $url"
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
}
$hash = (Get-FileHash $archive -Algorithm SHA256).Hash.ToLowerInvariant()
Write-Host "$name sha256 $hash"
if ($Sha256 -and $hash -ne $Sha256.ToLowerInvariant()) {
    throw "SHA-256 mismatch for ${name}: expected $Sha256, got $hash"
}

$sevenZip = (Get-Command 7z -ErrorAction SilentlyContinue).Source
if (-not $sevenZip) { $sevenZip = "C:\Program Files\7-Zip\7z.exe" }
$x = Join-Path $dl "x"
if (Test-Path $x) { Remove-Item -Recurse -Force $x }
& $sevenZip x $archive "-o$x" "x64\*" -y | Out-Null
if ($LASTEXITCODE -ne 0) { throw "7-Zip failed ($LASTEXITCODE)" }

New-Item -ItemType Directory -Force $Dest | Out-Null
$found = 0
foreach ($dll in "opengl32.dll", "libgallium_wgl.dll", "libglapi.dll") {
    $src = Join-Path $x "x64\$dll"
    if (Test-Path $src) {
        Copy-Item $src $Dest -Force
        $found++
        Write-Host "  $dll"
    }
}
if (-not (Test-Path (Join-Path $Dest "opengl32.dll")) -or -not (Test-Path (Join-Path $Dest "libgallium_wgl.dll"))) {
    Write-Host "Archive x64 contents:"
    Get-ChildItem (Join-Path $x "x64") | ForEach-Object { Write-Host "  $($_.Name)" }
    throw "opengl32.dll / libgallium_wgl.dll not found in $name"
}
Write-Host "Mesa ${Version}: $found DLLs in $Dest"
