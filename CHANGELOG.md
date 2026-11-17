# Changelog

## 2.0.0 (in progress)

Lumen becomes an image quality workbench: whole camera comparison rounds,
not one comparison at a time.

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

### Engineering
- CI checks every work branch: unit tests with and without FFmpeg, clippy, a full debug build.
- Report writers render any slide list; the web page version of decks keeps the slide layout.
