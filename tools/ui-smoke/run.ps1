<#
.SYNOPSIS
  Lumen UI smoke test: drives lumen.exe through the dev hook and saves screenshots.

.DESCRIPTION
  For every tools/ui-smoke/scenarios/*.txt (sorted by name) this starts a fresh
  lumen.exe (portable mode, empty settings) with LUMEN_DEV_DIR set, sends the
  scenario's dev hook commands one at a time through cmd.txt and waits for
  done.txt after each. Screenshots (`shot name.png`) land in <Out>\shots as
  <scenario>__name.png; logs per scenario in <Out>\logs\<scenario>.

  A scenario fails when Lumen exits early, a step times out (hang), a panic or
  crash is logged, a screenshot is missing, the dev hook reports an unknown
  command or key, or Lumen does not exit after `run Exit`. The script exits 1
  when any scenario failed (a scenario marked `@known-failure <reason>` is
  reported as known-fail but does not fail the run).

.EXAMPLE
  pwsh tools/ui-smoke/run.ps1 -Exe target\debug\lumen.exe -Mesa mesa -Media ui-smoke-media -Out ui-smoke-out
#>
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$Media,
    [Parameter(Mandatory = $true)][string]$Out,
    # Folder with Mesa's opengl32.dll + libgallium_wgl.dll (get_mesa.ps1); empty uses the system OpenGL.
    [string]$Mesa = "",
    [string]$Scenarios = (Join-Path $PSScriptRoot "scenarios"),
    # Only run scenarios whose file name contains one of these strings.
    [string[]]$Only = @(),
    [int]$Width = 1280,
    [int]$Height = 800,
    [int]$StartTimeout = 120,
    [int]$StepTimeout = 60,
    [switch]$SetResolution
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

function Abs([string]$p) { return [System.IO.Path]::GetFullPath([System.IO.Path]::Combine((Get-Location).Path, $p)) }
$Exe = Abs $Exe; $Media = Abs $Media; $Out = Abs $Out; $Scenarios = Abs $Scenarios
if ($Mesa) { $Mesa = Abs $Mesa }

if (Test-Path $Out) { Remove-Item -Recurse -Force $Out }
$shots = Join-Path $Out "shots"
$logs = Join-Path $Out "logs"
New-Item -ItemType Directory -Force $shots, $logs | Out-Null
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("lumen-ui-smoke-" + [System.Guid]::NewGuid().ToString("N").Substring(0, 8))
$app = Join-Path $work "app"
New-Item -ItemType Directory -Force $app | Out-Null

# The app folder: lumen.exe, Mesa's OpenGL beside it (found before the system one), portable mode.
Copy-Item $Exe (Join-Path $app "lumen.exe")
# Symbols for readable panic backtraces, when the build's PDB sits beside the exe.
$pdb = [System.IO.Path]::ChangeExtension($Exe, ".pdb")
if (Test-Path $pdb) { Copy-Item $pdb (Join-Path $app "lumen.pdb") }
if ($Mesa) {
    Get-ChildItem $Mesa -Filter *.dll | Copy-Item -Destination $app
    $env:GALLIUM_DRIVER = "llvmpipe"
}
Set-Content -Path (Join-Path $app "portable.txt") -Value "" -Encoding ascii
$env:LUMEN_LOG = "info"
$env:RUST_BACKTRACE = "1"

function Desktop-Shot([string]$path) {
    try {
        $b = [System.Windows.Forms.SystemInformation]::VirtualScreen
        $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
        $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $g.Dispose(); $bmp.Dispose()
    } catch {
        Write-Host "  (desktop screenshot failed: $($_.Exception.Message))"
    }
}

if ($SetResolution) {
    Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class SmokeDisplay {
  [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
  public struct DEVMODE {
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
    public short dmSpecVersion, dmDriverVersion, dmSize, dmDriverExtra;
    public int dmFields, dmPositionX, dmPositionY, dmDisplayOrientation, dmDisplayFixedOutput;
    public short dmColor, dmDuplex, dmYResolution, dmTTOption, dmCollate;
    [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
    public short dmLogPixels;
    public int dmBitsPerPel, dmPelsWidth, dmPelsHeight, dmDisplayFlags, dmDisplayFrequency;
    public int dmICMMethod, dmICMIntent, dmMediaType, dmDitherType, dmReserved1, dmReserved2, dmPanningWidth, dmPanningHeight;
  }
  [DllImport("user32.dll", CharSet = CharSet.Ansi)] static extern bool EnumDisplaySettings(string dev, int mode, ref DEVMODE dm);
  [DllImport("user32.dll", CharSet = CharSet.Ansi)] static extern int ChangeDisplaySettings(ref DEVMODE dm, int flags);
  public static string Set(int w, int h) {
    DEVMODE dm = new DEVMODE(); dm.dmSize = (short)Marshal.SizeOf(typeof(DEVMODE));
    if (!EnumDisplaySettings(null, -1, ref dm)) return "EnumDisplaySettings failed";
    string before = dm.dmPelsWidth + "x" + dm.dmPelsHeight;
    if (dm.dmPelsWidth >= w && dm.dmPelsHeight >= h) return before + " (kept)";
    dm.dmPelsWidth = w; dm.dmPelsHeight = h; dm.dmFields = 0x80000 | 0x100000;
    return before + " -> " + w + "x" + h + ": ChangeDisplaySettings " + ChangeDisplaySettings(ref dm, 0);
  }
}
"@
    try { Write-Host "Display: $([SmokeDisplay]::Set(1920, 1080))" } catch { Write-Host "Display: could not change ($($_.Exception.Message))" }
}
$vs = [System.Windows.Forms.SystemInformation]::VirtualScreen
Write-Host "Desktop $($vs.Width)x$($vs.Height); window ${Width}x${Height}; media $Media"

$hasVideo = @(Get-ChildItem (Join-Path $Media "Video") -Filter *.mp4 -ErrorAction SilentlyContinue).Count -ge 2
$mediaFwd = $Media
$results = @()

foreach ($file in Get-ChildItem $Scenarios -Filter *.txt | Sort-Object Name) {
    $name = $file.BaseName
    if ($Only.Count -gt 0 -and -not ($Only | Where-Object { $name -like "*$_*" })) { continue }
    $res = [ordered]@{ name = $name; status = "pass"; errors = @(); shots = @(); seconds = 0 }
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    $slog = Join-Path $logs $name
    New-Item -ItemType Directory -Force $slog | Out-Null

    # Parse the scenario: directives (@...), comments (#), {media}, shot names.
    $stepTimeout = $StepTimeout
    $needs = @()
    $known = ""
    $steps = @("size $Width $Height", "wait 1500")
    foreach ($raw in Get-Content $file.FullName) {
        $line = $raw.Trim()
        if ($line -eq "" -or $line.StartsWith("#")) { continue }
        if ($line.StartsWith("@")) {
            $d = $line.Substring(1).Split(" ", 2)
            switch ($d[0]) {
                "timeout" { $stepTimeout = [int]$d[1] }
                "needs" { $needs += $d[1].Trim() }
                "known-failure" { $known = $d[1].Trim() }
                default { throw "${name}: unknown directive $line" }
            }
            continue
        }
        $line = $line.Replace("{media}", $mediaFwd)
        if ($line -match '^shot\s+(.+)$') {
            $shotName = "${name}__$($Matches[1].Trim())"
            if (-not $shotName.EndsWith(".png")) { $shotName += ".png" }
            $res.shots += $shotName
            $line = "shot " + (Join-Path $shots $shotName)
        }
        $steps += $line
    }
    $steps += "state"
    if ($needs -contains "video" -and -not $hasVideo) {
        Write-Host "== ${name}: skipped (no test videos)"
        $res.status = "skipped"; $res.shots = @()
        $results += [pscustomobject]$res
        continue
    }

    Write-Host "== $name ($($steps.Count) steps)"
    $dev = Join-Path $work "dev-$name"
    New-Item -ItemType Directory -Force $dev | Out-Null
    $data = Join-Path $app "LumenData"
    if (Test-Path $data) { Remove-Item -Recurse -Force $data }
    $env:LUMEN_DEV_DIR = $dev
    $stdout = Join-Path $slog "stdout.txt"
    $stderr = Join-Path $slog "stderr.txt"
    $p = Start-Process -FilePath (Join-Path $app "lumen.exe") -WorkingDirectory $app -PassThru -NoNewWindow `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $null = $p.Handle  # keeps ExitCode available after exit
    $stepsLog = New-Object System.Collections.Generic.List[string]

    $i = 0
    foreach ($step in $steps) {
        $i++
        $limit = if ($i -eq 1) { $StartTimeout } else { $stepTimeout }
        if ($step -match '^wait\s+(\d+)') { $limit += [int]$Matches[1] / 1000 }
        if ($step -match '^bench\s+(\d+)') { $limit += [int]$Matches[1] / 10 }
        $done = Join-Path $dev "done.txt"
        if (Test-Path $done) { Remove-Item $done -Force }
        $tmp = Join-Path $dev "cmd.tmp"
        [System.IO.File]::WriteAllText($tmp, "$step`n")
        Move-Item $tmp (Join-Path $dev "cmd.txt") -Force
        $t0 = [System.Diagnostics.Stopwatch]::StartNew()
        $outcome = "ok"
        while (-not (Test-Path $done)) {
            if ($p.HasExited) { $outcome = "exited"; break }
            if ($t0.Elapsed.TotalSeconds -gt $limit) { $outcome = "timeout"; break }
            Start-Sleep -Milliseconds 100
        }
        $msg = "{0,3} {1,-8} {2,6:N1}s  {3}" -f $i, $outcome, $t0.Elapsed.TotalSeconds, $step
        $stepsLog.Add($msg)
        if ($outcome -ne "ok") {
            Write-Host "  $msg"
            if ($outcome -eq "timeout") {
                $resp = try { $p.Responding } catch { "?" }
                $res.errors += "step $i timed out after ${limit}s (window responding: $resp): $step"
            } else {
                $res.errors += "Lumen exited during step ${i}: $step"
            }
            break
        }
        if ($step -match '^shot\s+(.+)$') {
            # The screenshot is written by a callback a frame or two later.
            $png = $Matches[1]
            $w = [System.Diagnostics.Stopwatch]::StartNew()
            while (-not (Test-Path $png) -and $w.Elapsed.TotalSeconds -lt 10 -and -not $p.HasExited) { Start-Sleep -Milliseconds 100 }
            if (-not (Test-Path $png)) { $res.errors += "screenshot not written: $(Split-Path $png -Leaf)" }
        }
    }

    Desktop-Shot (Join-Path $slog "desktop.png")
    if (-not $p.HasExited) {
        if ($res.errors.Count -eq 0) {
            $done = Join-Path $dev "done.txt"
            if (Test-Path $done) { Remove-Item $done -Force }
            [System.IO.File]::WriteAllText((Join-Path $dev "cmd.tmp"), "run Exit`n")
            Move-Item (Join-Path $dev "cmd.tmp") (Join-Path $dev "cmd.txt") -Force
            if (-not $p.WaitForExit(30000)) { $res.errors += "Lumen did not exit within 30 s of 'run Exit'" }
        }
        if (-not $p.HasExited) {
            Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
            $p.WaitForExit(10000) | Out-Null
        }
    } elseif ($res.errors.Count -eq 0) {
        $res.errors += "Lumen exited before the scenario ended"
    }
    $code = try { $p.ExitCode } catch { $null }
    $stepsLog.Add("exit code: $code")
    $stepsLog | Set-Content (Join-Path $slog "steps.txt") -Encoding utf8

    # Collect logs.
    foreach ($f in "lumen.log", "lumen.old.log", "crash.log", "crash.old.log") {
        $src = Join-Path $data $f
        if (Test-Path $src) { Copy-Item $src $slog }
    }
    $state = Join-Path $dev "state.txt"
    if (Test-Path $state) { Copy-Item $state $slog }

    # Verdict.
    if ($null -ne $code -and $code -ne 0 -and $res.errors.Count -eq 0) { $res.errors += "exit code $code" }
    if ((Test-Path (Join-Path $slog "crash.log")) -and (Get-Item (Join-Path $slog "crash.log")).Length -gt 0) {
        $res.errors += "crash.log written: " + ((Get-Content (Join-Path $slog "crash.log") -TotalCount 3) -join " | ")
    }
    foreach ($f in "stderr.txt", "stdout.txt", "lumen.log") {
        $path = Join-Path $slog $f
        if (Test-Path $path) {
            $hit = Select-String -Path $path -Pattern "panicked at|thread '.*' panicked" -List
            if ($hit) { $res.errors += "panic in ${f}: $($hit.Line.Trim())" }
        }
    }
    if (Test-Path $state) {
        foreach ($l in Select-String -Path $state -Pattern "^unknown (command|key) ") { $res.errors += "dev hook: $($l.Line.Trim())" }
    }
    if ($res.errors.Count -gt 0) { $res.status = "fail" }
    if ($known) {
        # A bug already reported: show it, do not fail the job; say so once it is fixed.
        $res.known = $known
        if ($res.status -eq "fail") {
            $res.status = "known-fail"
            Write-Host "  known failure ($known)"
        } else {
            Write-Host "  NOTE: passes now; remove '@known-failure' from $($file.Name)"
        }
    }
    $res.seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    foreach ($e in $res.errors) { Write-Host "  FAIL: $e" }
    Write-Host ("  {0} in {1}s, {2} screenshots" -f $res.status, $res.seconds, @($res.shots | Where-Object { Test-Path (Join-Path $shots $_) }).Count)
    $results += [pscustomobject]$res
}

$results | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Out "results.json") -Encoding utf8
Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue

# Screenshot sanity checks (not blank), index.html and the job summary.
$py = Get-Command python -ErrorAction SilentlyContinue
$checkFailed = $false
if ($py) {
    & $py.Source (Join-Path $PSScriptRoot "check_shots.py") $Out
    if ($LASTEXITCODE -ne 0) { $checkFailed = $true }
} else {
    Write-Host "python not found: screenshot checks and index.html skipped"
}

$failed = @($results | Where-Object { $_.status -eq "fail" })
$skipped = @($results | Where-Object { $_.status -eq "skipped" })
$knownFails = @($results | Where-Object { $_.status -eq "known-fail" })
Write-Host ""
Write-Host ("UI smoke: {0} scenarios, {1} failed, {2} known failures, {3} skipped" -f $results.Count, $failed.Count, $knownFails.Count, $skipped.Count)
if ($failed.Count -gt 0 -or $checkFailed -or $results.Count -eq 0) { exit 1 }
exit 0
