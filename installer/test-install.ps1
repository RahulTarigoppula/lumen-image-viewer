<#
.SYNOPSIS
  Silent install / upgrade / uninstall test for Lumen-Setup.exe (CI).
.DESCRIPTION
  Installs for the current user into a temp folder with a space in its name,
  with the file-type and desktop-icon tasks, and checks the files, the
  uninstall entry, the shortcuts and the file-type registration. Installs again
  over it (an upgrade must reuse the same entry), then uninstalls silently and
  checks that all of it is gone. Exits with 1 and prints the Setup logs on any
  missing piece. Touches the current user's registry: run it on a CI runner or
  a test machine without Lumen installed.
.EXAMPLE
  installer/test-install.ps1 -Setup dist/Lumen-Setup.exe
#>
param(
    [Parameter(Mandatory)] [string] $Setup,
    # Expected DisplayVersion; default: the workspace version in Cargo.toml.
    [string] $Version
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$setupPath = (Resolve-Path $Setup).Path

if (-not $Version) {
    $m = Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    $Version = $m.Matches[0].Groups[1].Value
}
$iss = Get-Content (Join-Path $PSScriptRoot "lumen.iss") -Raw
if ($iss -notmatch 'AppId=\{\{([0-9A-Fa-f-]{36})\}') { throw "AppId not found in lumen.iss" }
$appId = "{$($Matches[1])}"

$temp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$work = Join-Path $temp "lumen-install-test"
$dir = Join-Path $work "Lumen Test App"
$logs = Join-Path $work "logs"
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force $logs | Out-Null

$uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\${appId}_is1"
$startMenu = Join-Path ([Environment]::GetFolderPath("Programs")) "Lumen.lnk"
$desktop = Join-Path ([Environment]::GetFolderPath("Desktop")) "Lumen.lnk"
$classes = "HKCU:\Software\Classes"
$exe = Join-Path $dir "Lumen.exe"

$script:failures = @()
function Check([bool] $ok, [string] $what) {
    if ($ok) { Write-Host "  ok       $what" } else { Write-Host "  MISSING  $what" -ForegroundColor Red; $script:failures += $what }
}
function Stop-IfFailed([string] $phase) {
    if ($script:failures.Count -eq 0) { return }
    Write-Host "`n$phase failed: $($script:failures.Count) check(s):" -ForegroundColor Red
    $script:failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    foreach ($log in Get-ChildItem $logs -File -ErrorAction SilentlyContinue) {
        Write-Host "`n===== $($log.Name) =====" ; Get-Content $log.FullName | Select-Object -Last 80
    }
    Write-Host "::error::Installer test: $phase failed ($($script:failures -join '; '))"
    exit 1
}
function Get-RegValue([string] $key, [string] $name) {
    $k = Get-Item -LiteralPath $key -ErrorAction SilentlyContinue
    if ($k) { return $k.GetValue($name) } else { return $null }
}
function Get-ShortcutTarget([string] $lnk) {
    if (-not (Test-Path -LiteralPath $lnk)) { return $null }
    return (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath
}
function Invoke-Setup([string] $file, [string] $arguments, [string] $what) {
    Write-Host "> $file $arguments"
    $p = Start-Process -FilePath $file -ArgumentList $arguments -Wait -PassThru
    Check ($p.ExitCode -eq 0) "$what exit code 0 (got $($p.ExitCode))"
}
function Test-Installed([string] $phase) {
    Write-Host "`nChecking after ${phase}:"
    foreach ($f in "Lumen.exe", "LICENSE.txt", "THIRD-PARTY-NOTICES.md", "unins000.exe", "unins000.dat") {
        Check (Test-Path -LiteralPath (Join-Path $dir $f)) "file $f"
    }
    Check (-not (Test-Path -LiteralPath (Join-Path $dir "unins001.exe"))) "only one uninstaller (upgrade in place, no unins001.exe)"
    Check (-not (Test-Path -LiteralPath (Join-Path $dir "portable.txt"))) "no portable.txt in the install folder"
    Check (Test-Path -LiteralPath $uninstallKey) "uninstall entry $uninstallKey"
    Check ((Get-RegValue $uninstallKey "DisplayName") -eq "Lumen") "uninstall DisplayName = Lumen"
    Check ((Get-RegValue $uninstallKey "DisplayVersion") -eq $Version) "uninstall DisplayVersion = $Version"
    Check ((Get-RegValue $uninstallKey "Publisher") -eq "Rahul Tarigoppula") "uninstall Publisher"
    Check ("$(Get-RegValue $uninstallKey 'InstallLocation')".TrimEnd('\') -eq $dir) "uninstall InstallLocation = $dir"
    Check ("$(Get-RegValue $uninstallKey 'UninstallString')" -like "*unins000.exe*") "uninstall UninstallString"
    Check ((Get-ShortcutTarget $startMenu) -eq $exe) "Start menu shortcut $startMenu -> $exe"
    Check ((Get-ShortcutTarget $desktop) -eq $exe) "desktop shortcut $desktop -> $exe"
    # The file-type task: lumen.exe --register-file-types (shell.rs).
    Check ("$(Get-RegValue "$classes\Lumen.Image\shell\open\command" '')" -like "*$exe*") "Lumen.Image open command points at $exe"
    Check ("$(Get-RegValue "$classes\Lumen.Video\shell\open\command" '')" -like "*$exe*") "Lumen.Video open command points at $exe"
    Check ($null -ne (Get-RegValue "$classes\.jpg\OpenWithProgids" "Lumen.Image")) ".jpg Open with lists Lumen.Image"
    Check ($null -ne (Get-RegValue "$classes\.mp4\OpenWithProgids" "Lumen.Video")) ".mp4 Open with lists Lumen.Video"
    Check ((Get-RegValue "HKCU:\Software\RegisteredApplications" "Lumen") -eq "Software\Lumen\Capabilities") "listed in Default apps (RegisteredApplications)"
    Stop-IfFailed $phase
}

Write-Host "Lumen $Version, AppId $appId, setup $setupPath"
Write-Host "Install folder: $dir"
Check (-not (Test-Path -LiteralPath $uninstallKey)) "Lumen is not installed before the test"
Check (-not (Test-Path -LiteralPath "$classes\Lumen.Image")) "Lumen file types are not registered before the test"
Stop-IfFailed "precondition"

$common = "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CURRENTUSER /DIR=`"$dir`" /TASKS=`"fileassoc,desktopicon`""
$sw = [Diagnostics.Stopwatch]::StartNew()
Invoke-Setup $setupPath "$common /LOG=`"$logs\install.log`"" "install"
Write-Host ("install took {0:N1} s" -f $sw.Elapsed.TotalSeconds)
Test-Installed "install"

$sw.Restart()
Invoke-Setup $setupPath "$common /LOG=`"$logs\upgrade.log`"" "upgrade (install over the same version)"
Write-Host ("upgrade took {0:N1} s" -f $sw.Elapsed.TotalSeconds)
Test-Installed "upgrade"

# Stands in for the user's settings: uninstalling must leave %APPDATA%\Lumen alone.
$keep = Join-Path $env:APPDATA "Lumen\installer-test-keep.txt"
$keepExisted = Test-Path -LiteralPath $keep
New-Item -ItemType File -Force $keep | Out-Null

$uninstaller = "$(Get-RegValue $uninstallKey 'UninstallString')".Trim('"')
$sw.Restart()
Invoke-Setup $uninstaller "/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /LOG=`"$logs\uninstall.log`"" "uninstall"
# The uninstaller finishes from a copy in %TEMP%: wait for it to remove the folder.
$deadline = (Get-Date).AddSeconds(120)
while ((Test-Path -LiteralPath $dir) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Write-Host ("uninstall took {0:N1} s" -f $sw.Elapsed.TotalSeconds)

Write-Host "`nChecking after uninstall:"
Check (-not (Test-Path -LiteralPath $dir)) "install folder removed"
Check (-not (Test-Path -LiteralPath $uninstallKey)) "uninstall entry removed"
Check (-not (Test-Path -LiteralPath $startMenu)) "Start menu shortcut removed"
Check (-not (Test-Path -LiteralPath $desktop)) "desktop shortcut removed"
# lumen.exe --unregister (shell.rs unregister_all).
Check (-not (Test-Path -LiteralPath "$classes\Lumen.Image")) "Lumen.Image removed"
Check (-not (Test-Path -LiteralPath "$classes\Lumen.Video")) "Lumen.Video removed"
Check ($null -eq (Get-RegValue "$classes\.jpg\OpenWithProgids" "Lumen.Image")) ".jpg Open with no longer lists Lumen"
Check ($null -eq (Get-RegValue "HKCU:\Software\RegisteredApplications" "Lumen")) "removed from Default apps"
Check (-not (Test-Path -LiteralPath "HKCU:\Software\Lumen\Capabilities")) "Software\Lumen\Capabilities removed"
Check (Test-Path -LiteralPath $keep) "settings folder %APPDATA%\Lumen kept"
if (-not $keepExisted) { Remove-Item -LiteralPath $keep -ErrorAction SilentlyContinue }
Stop-IfFailed "uninstall"

Write-Host "`nInstaller test passed: install, upgrade in place and uninstall."
