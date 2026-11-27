# UI smoke test

Starts the freshly built `lumen.exe`, drives it through scripted scenarios with
the dev automation hook (`src/devhook.rs`) and saves screenshots and logs. It
runs in CI after every push to `dev/**` (job `ui-smoke` in
`.github/workflows/dev.yml`) and fails when Lumen crashes, hangs, logs a panic,
does not exit cleanly, or a screenshot is missing or blank.

## Results

Download the `ui-smoke` artifact of the run (Actions page, or
`gh run download <run id> -n ui-smoke -D <empty folder>`):

| File | Contents |
|---|---|
| `overview.png` | Contact sheet of every screenshot, captioned |
| `index.html` | All screenshots by scenario, with each scenario's result and errors |
| `shots/<scenario>__<name>.png` | Full-size window screenshots |
| `logs/<scenario>/steps.txt` | Every step with outcome and time, and Lumen's exit code |
| `logs/<scenario>/lumen.log`, `crash.log` | Lumen's own logs (`LUMEN_LOG=info`) |
| `logs/<scenario>/stdout.txt`, `stderr.txt` | Console output (panics with backtraces land here) |
| `logs/<scenario>/state.txt` | Output of `state` and other dev hook replies |
| `logs/<scenario>/desktop.png` | The whole desktop after the scenario (window placement, error dialogs) |
| `results.json` | Machine-readable results |

The job summary on the run page has a table of scenarios and problems.

## How it works

1. `get_mesa.ps1` downloads a pinned [mesa-dist-win](https://github.com/pal1000/mesa-dist-win)
   release and extracts the x64 `opengl32.dll` and `libgallium_wgl.dll`. Hosted
   runners have no GPU; copied next to `lumen.exe` with `GALLIUM_DRIVER=llvmpipe`
   they give a software OpenGL 4.5 core context that eframe's glow backend uses.
2. `make_media.py` generates deterministic test media: a camera test project
   (`RoundA/DeviceX|DeviceY|DeviceZ/scene_01..03.jpg`, the same three scenes
   framed, colored, blurred and noised differently per device, with EXIF make,
   model, ISO and exposure), a `Flat/` folder (gradient PNG, noise JPEG, text
   card, tall portrait, icon with alpha), `Compare2/` and `Compare3/` (two and
   three versions of scene 01, for "select all, compare"), `Undo/` (three
   photos that a scenario deletes and restores), `Explorer/` (a
   `Source/` folder with three pictures, an empty `Target/` and `Outside/` with
   one file, for cut / paste and drops from another program) and `Video/` with two
   4 s clips when an ffmpeg binary is found (PATH, or the `imageio-ffmpeg` pip
   package that CI installs).
3. `run.ps1` copies `lumen.exe` and the Mesa DLLs into a temporary folder with
   `portable.txt` (settings and logs then live in `LumenData` beside the exe,
   not in `%APPDATA%`). For each scenario it starts a fresh Lumen with empty
   settings and `LUMEN_DEV_DIR` set, sends the scenario one command at a time
   (writes `cmd.txt`, waits for `done.txt`), then sends `run Exit` and waits for
   the process to end. Every scenario starts with `size 1280 800` and
   `wait 1500`, and ends with `state`.
4. `check_shots.py` checks that every screenshot exists and is not a single flat
   color, and writes `index.html`, `overview.png` and the job summary.

## Adding a scenario

Add a file `scenarios/NN-name.txt` (run in name order). Each line is one dev
hook command (full list at the top of `src/devhook.rs`):

```text
# Comment
@needs video          # skip when no test videos were generated
@timeout 120          # seconds per step (default 60; a `wait` adds its own time)
@known-failure <why>  # a reported bug: shown as known-fail, does not fail the job
open {media}\RoundA\DeviceX\scene_01.jpg
wait 2000
run ToggleInfo        # any `Command` variant name from src/ui/commands.rs
key Ctrl+Q            # any shortcut the keymap parser accepts
click 640 400         # window point in points; `click x y right` for the right button
drag 300 300 420 400
type compare
ppp 1.5               # pretend the window moved to a 150% display
size 800 600
shot info.png         # saved as shots/NN-name__info.png
```

`{media}` is replaced with the absolute path of the generated media folder.
Give asynchronous work (folder listing, decoding, metrics, video) a `wait`
before the `shot`. An unknown `run` command or `key` fails the scenario, so
typos do not go unnoticed. When a `@known-failure` scenario starts passing,
the log says so; remove the directive then so it guards against regressions.

Panic backtraces in `stderr.txt` and `crash.log` have function names and
lines because the check job also uploads `lumen.pdb` (artifact
`lumen-dev-pdb`) and `run.ps1` puts it beside the exe.

## Running it yourself

On a Windows machine that can run the build:

```powershell
pwsh tools/ui-smoke/get_mesa.ps1 -Dest mesa     # optional: only without a usable GPU driver
python -m pip install pillow numpy imageio-ffmpeg
python tools/ui-smoke/make_media.py ui-smoke-media
pwsh tools/ui-smoke/run.ps1 -Exe target\debug\lumen.exe -Media ui-smoke-media -Out ui-smoke-out [-Mesa mesa] [-Only compare,video]
```

The window appears and is driven on the desktop; do not use the mouse over it
while a scenario runs. `-SetResolution` raises the desktop to 1920 x 1080 when
it is smaller (CI runners start at 1024 x 768).
