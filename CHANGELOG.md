# Changelog

## 2.1.0 (27 November 2026)

A full pass over looks, ease of use, stability and speed, and the FastStone-style way to arrange a folder by
hand.

### Browser
- Arrange by hand, like FastStone: drag thumbnails (or rows in the details view, or the viewer's filmstrip) between others to put them in any
  order. A line shows where they will land; dropping on the empty area puts them last; the view scrolls when the
  pointer is held near the top or bottom. Ctrl+Alt+Left / Right move the selected files one place, Ctrl+Z undoes an
  arrangement. The viewer, slideshow, compare, batch tools and exports follow the arranged order.
- "Custom order" in the sort menu; the order is kept per folder in Lumen's private data (files are never renamed or
  touched), follows renames, and can be made permanent with "Number the files in this order" (batch rename) or
  reset from the same menu.
- Folder thumbnails show the first pictures inside each folder.
- Custom order is per folder: arranging one folder no longer changes how other folders are sorted.
- Include subfolders (Show menu): every picture and video under a folder in one view, e.g. a phone's DCIM folder.
- Rubber-band selection: drag on empty space to select a box of thumbnails (Ctrl adds).
- Filters: color label chips, "Select the files shown" and "Clear filters"; Select flagged files (Ctrl+Shift+F).
- Ctrl+L, Alt+D or F4 to type a folder path; mouse back / forward buttons go through folders.
- A short window keeps room for the folder tree; the sort button keeps its label when space is short.

### Viewer
- Fit leaves room for the caption and the bottom bar, so the whole picture is visible in a window.
- The caption shows rating, flag and color label; flagging and labelling show a short notice.
- Optional: next image after rating, flagging or labelling (Settings, Viewer), for fast culling.
- Past the last image a notice appears; pressing again opens the next folder with pictures.
- Copy text in image (Ctrl+T): reads the text with OCR and puts it on the clipboard.
- Ctrl+V saves the clipboard picture as a new PNG next to the current one instead of replacing its pixels.
- Edit tools (Resize, Canvas, Border, Rotate, Levels, Curves, Sharpen, Blur, Effects, Adjust colors, Text,
  Watermark) open with the values used last time; Reset goes back to the defaults. Save as remembers the format,
  its options and the folder.

### Compare, analysis and workbench
- Pane colors no longer depend on the accent color (A is always blue), and pane letters stay readable on every color.
- Long file names are shortened in the middle, so the part that differs (the device) stays visible.
- The Metadata window keeps its size and aligns its columns; its third tab is now "File & encoding".
- Blind tests hide every control that could reveal or change a side; the line profile sits in the middle of the view.
- Analysis values and their "Show where" links line up in columns; links stay readable in the light theme.
- Workbench: measured values in a grid with units, one reference hint, aligned columns, correct plurals.
- Deleting a pane's file in Compare keeps device names, colors and alignment with the right panes.
- Video: no repeated timecode or audio notice over the picture.

### General
- A welcome card on first start: FastStone-compatible keys and what the mouse wheel does.
- The window size is remembered between runs.

### Stability and speed
- Saving in place asks first when the file changed on disk since it was opened (e.g. edited in another program);
  undoing a rotation skips files changed since.
- Moves copy and delete only across drives (a locked file is no longer duplicated); copies are written under a
  temporary name and renamed into place when complete, so quitting never leaves a half-written file.
- Deleting on a network or removable drive (no Recycle Bin) is confirmed and reported as permanent.
- Batch convert with Overwrite skips animated, multi-page and high bit depth originals instead of flattening them;
  batch rename keeps one recovery journal per run.
- Slideshow with subfolders searches in the background; rotating lossless WebP in the browser stays lossless.
- The viewer's filmstrip lays out and loads only the thumbnails in view (fast with 10,000 images), and thumbnails
  near the current image load first.
- Thumbnails on screen are never thrown out and reloaded (large or 4K windows with small thumbnails), uploads are
  spread over frames, and a JPEG thumbnail opens its file twice instead of four times.
- Images with a color profile get thumbnails by scaling first, then converting the small result.
- The viewer skips a second full decode for a "preview" when no cheap preview exists, and stops decoding images
  you have already stepped past; one worker always stays free for the image on screen.
- Metadata for files on screen is read first; EXIF of TIFF-based raw files no longer reads the whole file.
- Very large images are decoded for thumbnails at most two at a time, so a folder of panoramas cannot exhaust memory.
- Ratings, custom orders and settings are written on a background thread, atomically (a power cut cannot empty them).
- A damaged color label in the tags file no longer crashes the browser.
- The folder is checked for changes on a background thread, and the Move / Copy dialog no longer checks every
  recent folder on every frame (offline network drives froze the app).
- Folder thumbnails list only part of a large folder and can no longer get stuck loading.
- Arranging many selected files is no longer slow.

## 2.0.0 (23 November 2026)

Lumen becomes an image quality workbench: whole camera comparison rounds,
not one comparison at a time.

### Metadata and EXIF
- EXIF and Metadata buttons in the viewer toolbar and in compare (Ctrl+Shift+X, Ctrl+Shift+I; also in the browser for up
  to four selected files). One window shows every open image side by side, with differences marked, a filter, "only
  differences" and copy as TSV.
- Capture summary: camera and firmware, lens and a camera-module guess (ultra wide, main, tele, front), exposure and
  exposure value, metering, white balance, flash, and which phone features were used: multi-frame / HDR (with the
  source of each clue), HDR gain map (Ultra HDR, ISO 21496-1, Apple), night mode, portrait / depth, motion photo,
  Live Photo, panorama, burst, Single Take.
- Every EXIF tag grouped by IFD, with units and hover notes.
- Encoding: estimated JPEG quality from the quantization tables, chroma subsampling, progressive or baseline,
  optimized or standard Huffman tables, restart interval, C2PA, data after the image; PNG, WebP, HEIF/AVIF, GIF, BMP,
  JPEG XL and TIFF/DNG header facts.
- ICC profile, XMP (Google, Adobe and Samsung namespaces), Samsung trailer entries, multi-picture (MPF) images, maker
  note, PNG text chunks (including generation parameters) and comments. Damaged files give warnings, never a crash.
- Export metadata as CSV (browser right-click, Tools; main menu, Tools; command palette): one row per image with the
  capture summary, every EXIF tag and the file and encoding facts, for the selected images or the whole folder.

### Viewer and compare
- Pixel values under the pointer: RGB, hex, luma and Lab (Shift+V in the viewer; V in compare reads the same spot in
  every pane).
- Inspect views (Shift+O in the viewer, O in compare): clipping warnings, false color exposure, focus peaking, luma,
  red, green and blue channels.
- Line profile (P in compare): drag a line to plot brightness along it in every pane, with the 10-90% edge width of
  each.

### Workbench (camera comparison projects)
- Open a folder of captures as a project (Ctrl+Shift+W or Tools, Workbench). Devices and scenes are found
  automatically from folder layouts, file names or EXIF camera models; the project is saved as
  `lumen-project.json` with relative paths, so it can move or live on a shared drive.
- Overview grid: one row per scene, one column per device, with thumbnails, verdicts, scores, missing captures.
- Compare a scene with device names and colors on the panes; Page Up / Page Down for the other scenes.
- Scorecard panel: Win / Par / Lose against a reference device, attribute scores (1 to 5, or -3 to +3 against
  the reference), notes per device and a comment per scene.
- Artifact tags with severity on review marks (Halo, Noise, Flare, Banding, White balance and more).
- Blind test: random pairs with hidden names and sides, forced choice, Bradley-Terry ranking with 90% intervals.
- Round report: one PowerPoint, PDF or web page for the whole project (summary with Win / Par / Lose bars,
  scorecard by attribute, flagged artifacts, blind test ranking, every scene and its details). CSV of all scores.
- Project reviews (marks, comments, details) are stored with the project and survive moving the folder.
- Measure: objective sharpness, noise, visual noise, clipping, white balance and banding of every capture, each scaled
  to the same size so devices are compared fairly; means per device in the side panel, grid, CSV and report.
- Previous round: pick last round's project to see score changes per device and every score that went down.

### Compare
- Align to A (key A): feature matching (FAST corners, rotated BRIEF, RANSAC homography) warps B-D onto A so phones
  with different lenses and crops line up; every pane zooms to the field of view they share; the difference view
  uses the aligned images.

### Camera metrics
- Visual noise (CPIQ-style, in CIELAB, on flat areas only, gradients removed), with a tile map.
- White balance on near-neutral areas with a per-area map (color cast v2); clipping per channel near both ends with a
  tile map (exposure v4).
- Differences described in approximate just-noticeable differences ("B is about 2 JND noisier than A").

### Video
- Frame timing checks per video: measured and declared frame rate, variable frame rate, dropped frames with their
  places, repeated timestamps, likely duplicates, jitter and stutter, with a plain verdict and a CSV export.
- Auto-align start points: clips of the same scene from different phones are synchronized from the picture
  (brightness and motion signals, exposure-independent), with confidence and undo.

### AI review
- Rank devices: 2 to 4 images in one request (a labelled 2x2 grid when the server takes fewer images), with a verdict,
  a ranking with scores, strengths and weaknesses, and findings drawn on the right panes.
- Add findings as marks: AI findings become review marks with the matching artifact tag and severity.

### Speed
- Portrait phone photos open about 0.15 s faster at 24 MP (parallel tiled rotation, in-place flips, direct RGB to
  RGBA conversion, a faster display pyramid).
- Browser sorting of 10,000 files 4 to 7 times faster; status bar totals no longer hash every path each frame;
  thumbnail requests no longer lock the worker queue; capture dates read without formatting all EXIF tags.
- Median filter 5 to 8 times faster, small Gaussian blur and unsharp mask about 2 times faster.

### Engineering
- CI checks every work branch: unit tests with and without FFmpeg, clippy, a full debug build.
- Report writers render any slide list; the web page version of decks keeps the slide layout.
