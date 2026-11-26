# Lumen handoff

State on 27 November 2026: version 2.1.0, `main` and `dev/v1.4` at the same commit, every CI job green, releases
built by `.github/workflows/build.yml` (Lumen.exe, Lumen-portable.zip, SHA256SUMS.txt).

## 2.1.0 in short

A review of the whole app from four angles (looks and ease of use, stability, speed, missing FastStone features),
then fixes for what it found. Details in [CHANGELOG.md](../CHANGELOG.md):

- Arrange a folder by hand (drag thumbnails, rows or filmstrip cells; per folder; undo), folder thumbnails, rubber-band
  selection, Include subfolders, color label filter, Select flagged, address bar and mouse back / forward keys.
- Viewer: fit that keeps the picture clear of the bars, tags in the caption, next folder at the end, next image after
  rating (option), copy text in image (OCR), paste as a new file; edit tools remember their values.
- Safety: no silent overwrite of files changed on disk, staged copies, real moves only across drives, permanent
  deletes on network drives are said as such, batch Overwrite never flattens animations or 16-bit files.
- Speed: virtualized filmstrip, thumbnail cache that never evicts what is on screen, fewer file opens, cancelled
  decodes, background saving, bounded memory for huge images.
- Looks: fixed pane colors, readable letters, middle-shortened names, aligned columns in Metadata, Analysis and
  Workbench, a welcome card for FastStone users.

## What changed since 1.3

Lumen went from an image viewer with compare and analysis to a camera image quality workbench. Full list in
[CHANGELOG.md](../CHANGELOG.md); the short version:

| Area | New in 2.0 |
|---|---|
| Workbench | Projects from a folder of captures (devices and scenes detected), scene x device grid, scorecards (Win / Par / Lose, attribute scores), artifact tags, objective measurements per capture, previous-round comparison, blind test with Bradley-Terry ranking, one round report (PowerPoint, PDF, web) and CSV |
| Compare | Align to A (feature matching and homography), device names and colors, scene bar, pixel values in every pane, inspect views, line profile with edge width, EXIF and Metadata buttons |
| Metadata | Capture summary (camera module, exposure, multi-frame / HDR, gain map, night, portrait, motion photo, Live Photo), every EXIF tag, JPEG quality estimate and encoding facts, ICC, XMP, Samsung trailer, MPF, maker note, PNG text; CSV export of many files |
| Metrics | Visual noise (CIELAB, CPIQ-style), white balance on neutral areas, clipping per channel, differences in JND; fixes to blur and blocking false positives |
| Video | Frame timing (real frame rate, drops, duplicates, jitter), picture-based auto-align of start points |
| AI | Rank 2 to 4 devices in one request; findings become review marks |
| Speed | Faster opening of portrait phone photos, browser sorting, filters |
| Engineering | Dev CI on every `dev/**` branch, UI smoke test that drives the real app and saves screenshots |

## Usage flows

### 1. Run a camera comparison round
1. Put the captures in one folder: one subfolder per device (`Round1/PhoneA/night_01.jpg`), or the device in the
   file name (`night_01_PhoneA.jpg`). Mixed folders also work through EXIF camera models.
2. Browse to the folder and press **Ctrl+Shift+W** (Tools, Workbench). Check the detected devices and scenes in
   setup, pick the reference device, optionally pick last round's project to see changes.
3. The grid shows every scene x device; missing captures are marked. Click **Measure** to compute sharpness,
   noise, visual noise, exposure, white balance and banding for every capture.
4. Open a scene: Compare opens with device names. Press **A** to align the phones, score in the side panel,
   mark artifacts with **M** (tag and severity), comment with **C**. **Page Up / Page Down** moves between scenes.
5. Optional: **Blind test** for forced-choice pairs with hidden names; the ranking appears in the side panel.
6. **Export** the round report (PowerPoint, PDF or web page) and the CSV of scores.

Details: [WORKBENCH.md](WORKBENCH.md).

### 2. Check what the phone did and compare metadata
- Viewer: the **EXIF** and **Metadata** buttons in the toolbar (Ctrl+Shift+X, Ctrl+Shift+I). The Capture tab
  shows camera module, exposure and which features fired (HDR / multi-frame, gain map, night, portrait, motion photo).
- Compare: the same buttons show every pane side by side; differences are highlighted, **Only differences** hides
  the rest, the copy button puts the table on the clipboard.
- Browser: select up to 4 images and use the same shortcuts.
- Many files: Tools, **Export metadata of images as CSV** (selected images, or the whole folder when one or none is
  selected).

### 3. Inspect image quality by eye
- **Pixel values**: Shift+V in the viewer, V in compare (same spot in every pane): RGB, hex, Lab, luma.
- **Inspect views**: Shift+O in the viewer, O in compare: clipping, false color, focus peaking, luma, R, G, B.
- **Line profile**: P in compare, drag across an edge; the plot shows each pane and its 10-90% edge width
  (smaller is sharper).
- **Difference view**: D in compare (uses the aligned images after A).

### 4. Objective analysis
- **Ctrl+Q** opens the analysis panel, **Analyze** gives a plain-language summary (sharpness, noise, clipping,
  color cast, compression, how close B is to A); Expert shows every metric, heatmaps, region of interest (R), CSV.
- **AI review** (optional, off by default): set a server in Settings, AI review; **Rank devices** sends 2 to 4
  images and returns a ranking with findings drawn on the panes.

### 5. Arrange a folder by hand
- In the thumbnail grid (or details view), drag files between others; a line shows where they land. The sort switches
  to **Custom order**, kept per folder in Lumen's own data (the files are not touched).
- Ctrl+Alt+Left / Right move the selection one place; Ctrl+Z undoes the last arrangement.
- The viewer (Enter), slideshow, compare and batch tools use this order. To make it permanent in the file names, use
  Sort, **Number the files in this order** (batch rename with a counter); **Reset this folder's custom order** goes
  back to name order.

### 6. Cull fast
- In the viewer: 1-5 rate, Q flag, 6-9 color labels (red to blue; see F1); the caption shows the tags. Turn on Settings,
  Viewer, **Next image after rating** to move on automatically.
- Back in the browser: Show menu, **Flagged only** or a minimum rating or a color label, then **Select the files
  shown** (or Ctrl+Shift+F for flagged), then F7 / F8 to copy or move them.
- At the end of a folder the viewer says so; pressing Right again opens the next folder with pictures.

### 7. Video
- Select clips, Compare (P) or Ctrl+Shift+V. Frame timing checks find drops and duplicates; **Auto-align** syncs
  start points of clips from different phones; PSNR / SSIM and whole-file analysis are in the analysis panel.

## How to build and test

- Local builds on this PC are blocked by Windows Smart App Control (it refuses freshly built binaries). All
  building and testing runs in GitHub Actions.
- Push to any `dev/**` branch: `dev.yml` runs the text check, unit tests with and without FFmpeg, clippy, a debug
  build and the UI smoke test. The smoke artifact `ui-smoke` has a screenshot per step (`tools/ui-smoke/scenarios`,
  15 scenarios).
- Push to `main`: `build.yml` builds the release executable. A `v*` tag also publishes a GitHub release.
- Before committing UI text: `python tools/scrub_text.py --check src crates` (plain punctuation only).

## Known gaps (not verified or not done)

- The CI screenshots use synthetic test images. Real phone files (Samsung, Apple, Google, HEIC) have not been
  opened in the app by a person; the metadata reader's own tests build phone-style files byte by byte.
- Maker notes: only a few Apple fields are decoded; other makers show maker, size and format.
- Extended XMP, JPEG XL metadata in Brotli boxes and ICC in BMP headers are reported as present but not decoded.
- Raw files: pixel size comes from the first IFD, which can be a preview.
- The workbench has been exercised on small generated projects, not on a full real round with hundreds of captures.

- Not done from the 2.1 review: cut / paste with Explorer (Ctrl+X, the clipboard's move flag), dropping files from
  Explorer onto a folder to copy / move them, undo of a delete from the Recycle Bin, videos inline in the viewer and
  slideshow, lossless JPEG crop, the Windows share sheet.

## Next candidates

The items above, then region-aware metrics (faces, sky), HDR (Ultra HDR gain map display, HDR10 video), a company
report template, VMAF for video pairs, remappable Compare shortcuts, full maker-note decoding for Samsung and Google.
