# Changelog

## 2.2.0 (28 November 2026)

Everything left over from the 2.1 handoff (Explorer cut / paste and drop, undo of deletes, sharing, videos in the
viewer, lossless JPEG crop, an installer), Samsung and Pixel camera data with HDR gain maps, report branding, and a
second full pass over looks, ease of use, stability and speed.

### Install
- New Windows installer, Lumen-Setup.exe: installs for your account without admin rights (or for all users), adds a
  Start menu shortcut and an optional desktop icon, can make Lumen an "Open with" choice for pictures and videos,
  updates in place and uninstalls from Settings > Apps (your settings are kept).
- Lumen.exe and Lumen-portable.zip stay fully portable. SHA256SUMS.txt lists all three downloads in the standard
  format, so `sha256sum -c` works.
- Command-line switches `--register-file-types` and `--unregister` add or remove Lumen's Windows integration
  without opening a window.

### Files, Explorer and sharing
- Ctrl+X cuts files in the browser, like Explorer: they look faded until pasted; Ctrl+V in another folder (or in
  Explorer) moves them, and Ctrl+Z puts them back. Files cut in Explorer are moved when pasted in Lumen. A copy
  pasted into its own folder is named "photo - Copy.jpg".
- Drag files from Explorer onto a folder tile, list row or folder tree row to move them there (same drive) or copy
  them (other drive); Shift moves, Ctrl copies. The folder lights up and a badge says "Move to ..." or "Copy to ...".
  Ctrl or Shift on the empty area copies or moves into the folder shown; other drops still open the files. Lumen's
  own thumbnail drags onto folders follow the same rule.
- Copy and move messages say where the files went, e.g. "Moved 3 files to Holiday (Ctrl+Z to undo)".
- Undo a delete: Ctrl+Z after deleting in the browser or the viewer brings the files back from the Recycle Bin to
  where they were, selected and with their ratings and labels. A name now taken gets a number added, a missing
  folder is made again, and files that cannot be restored are listed.
- Share... (browser right-click menu, viewer menus, command palette) opens the Windows share window for the
  selected files or the current image.

### Browser
- When a filter hides every file, the grid says "No files match the filters" and offers Clear filters; the status
  bar says how many are hidden ("3 of 120 files (filtered)").
- Long names under thumbnails keep their start and extension ("IMG_2026...final_edit.jpg").
- A folder that does not exist in the address bar says "Folder not found" and keeps your text; a path to a file
  opens its folder with the file selected.
- The right-click menu now opens on files (before, only on empty space); it has Rotate left / right, and Ctrl+P
  prints. Comparing more than 4 selected images says it shows the first 4.
- The folder tree works from the keyboard: F6 / Shift+F6 move between the address bar, the folders and the files;
  arrows, Left / Right, Enter, Home / End and typing a name move around. Clicking a folder gives the tree the
  keyboard, clicking the files takes it back.
- Keyboard shortcuts (F1): a search box, and mouse actions listed for the browser, the viewer and Compare,
  following your mouse wheel setting.
- Moving through thumbnails with the keyboard no longer jumps the view when it is scrolled.

### Viewer
- Videos play right in the viewer, between the folder's images: poster first, then a bar for play / pause,
  seeking, repeat, volume and "Open in video mode". K plays or pauses, comma and period step one frame,
  Shift+Left / Right jump 5 seconds; Left / Right still change file. A single video opened from the browser or
  Explorer plays here (Settings > Viewer > Videos can send it to video mode); volume, mute and repeat are remembered.
- Slideshows include videos: they play muted (M for sound) and the show moves on when one ends or after a time you
  set. Builds without the video player show a clear card with "Open with the default app".
- Full screen shows the caption with rating, flag and color label by default; the old "Show info overlay" setting
  is now "Open the info panel on start".
- New optional camera line in the caption (exposure, aperture, ISO, focal length, date), in full screen by
  default; choose when and which fields in Settings > Viewer.
- Zoom navigator: when zoomed in, a small overview in the corner shows which part you see; drag it to pan (also in
  Compare; can be turned off in Settings > Viewer).
- Stars, flag and color labels look and work the same in the browser preview pane and the viewer's Info panel,
  with tooltips that name your own keys; the viewer's right-click menu has "Rate & tag". Stars are easier to see
  in the light theme, and hovering a star previews that rating.
- Ratings, flags, color labels and Compare's Keep can be undone with Ctrl+Z or the Undo button on their notice;
  notices say how many files changed.
- Edge panels: Full screen only (new default), Always or Off; resting the pointer at a window edge no longer pops a
  panel.
- The Info panel takes at most 40% of the window and hides by itself below 700 points wide.
- Going past the last picture opens the next folder at its first picture in that folder's own order, including a
  hand-arranged one.
- The EXIF and Metadata buttons are one Metadata button; the window opens on the tab you used last.

### Lossless JPEG
- Crop has a new "Lossless" option for JPEGs: it crops the file without re-saving the picture, so no quality is
  lost; the left and top edges snap to the JPEG's 8 or 16 px grid. Ctrl+Z undoes it.
- Lossless rotate and flip now also work for JPEGs whose metadata has no orientation tag: the picture itself is
  turned without re-saving (a few edge pixels may be trimmed, and the message says so).
- Lossless crops and turns update the picture size in the metadata and make a new thumbnail; an HDR gain map that
  no longer matches is removed, and you are told.
- Rotating in the viewer runs in the background and can still be undone with Ctrl+Z.

### Compare, analysis and reports
- A Show menu holds histograms, the camera strip, pixel values, inspect views, the pane layout and Metadata, so
  the view modes keep their names on smaller screens.
- Pictures are laid out in columns, rows or a grid, whichever shows them largest (or pick one in the Show menu).
- Compare shortcuts can be changed in Settings > Keyboard (new Compare and Blind test tabs) with conflict warnings
  and reset; the defaults are the same as before. The ? overlay, the shortcuts window, tooltips and menus show your
  own keys, and Compare's commands are in the command palette.
- Deleting a picture on a USB drive, SD card or network folder in Compare says it is permanent and always asks;
  elsewhere it follows the "Confirm delete" setting.
- Analysis no longer says "no problem areas" right under a list of things to check; Mark up keeps Clear and Done
  visible when the Review panel is open.
- Report branding: company or team name, logo, accent color and a footer line such as "Confidential", with a cover
  page showing the project, devices, date and author. Works for the round report and the comparison review, as
  PowerPoint, PDF and web page; set it once as the default ("Report branding..." in the palette) or per project.

### Camera metadata and HDR
- Samsung Galaxy photos: the maker note is decoded (device, firmware, lens, sensor area, Smart Range, exposure,
  ISO, face detection), and its clues appear in the capture summary.
- Google Pixel photos: HDR+, Night Sight, astrophotography and best-of-burst are recognized; Google's encrypted
  maker note and XMP blocks are labelled as encrypted instead of shown as text.
- HDR gain maps (Ultra HDR, ISO 21496-1, Apple JPEG): a new "HDR gain map" section shows boost, headroom and map
  size, and two new inspect views (Shift+O, in Compare O): "Gain map" over the picture, and "HDR rendition" with a
  screen headroom slider, to see which areas an HDR screen brightens. HEIC gain maps are detected and say so.

### Video mode
- Discarding an analysis or auto-align (Swap, closing a video, Re-run, leaving video mode) stops it at once.
- Opening videos no longer freezes the window; each file is read once to get its details.
- Stepping back no longer keeps a full-resolution copy of every remembered frame (much less memory for 4K pairs).
- After Swap or closing a video, quality numbers, the difference image and the timeline no longer briefly show
  the previous video's results; the Info panel stays fast during long analyses.
- With one video the caption shows just the name and time; Save frames no longer writes a test file.

### Stability and speed
- Blind test votes are saved after every vote, and the project is saved when Lumen closes.
- When someone else saves the project while you work, their changes are added to yours; if you both changed the
  same score or note, Lumen asks which to keep. Opening, saving and finding scenes again no longer freeze Lumen on
  a slow or offline share, and measurements always land in the scene they were measured from.
- A damaged review file is kept as .broken instead of being replaced by an empty review; overlapping review saves
  can no longer break the file; moving a detail box from a damaged review no longer crashes Lumen.
- The batch rename journal is written safely to disk, and an unreadable journal is kept instead of deleted.
- A JPEG next to a RAW file of the same name gets its own sidecar (name.jpg.xmp), so exporting its tags no longer
  overwrites the RAW's Lightroom rating.
- Saving, rotating and editing metadata work for files with very long paths or names.
- An error in a background task (EXIF panel, slideshow captions) no longer closes Lumen.
- Animated GIF, PNG and WebP files keep only as many frames as fit in memory (a small file with a huge canvas could
  make Lumen use about 4 GB); multi-page TIFFs show their first page right away and load the rest in the background.
- Reading metadata no longer reads whole files: big TIFF scans and files without EXIF are read only as far as their
  metadata, and each file's EXIF is read once instead of twice.
- SVG files can only show images embedded in them or stored in their own folder.
- The Metadata window no longer rebuilds its table every frame; Save As, the filmstrip, going to the next folder
  and Settings > Performance no longer freeze while a slow or network drive answers.
- Undo (Ctrl+Z) of a move, rename, rotation or lossless crop runs in the background with a progress card, so undoing
  a big move to another drive no longer freezes the window; files that could not be put back are named, and the ones
  back in the folder shown are selected.
- Save and Save as write in the background with a "Saving..." card. Opening another file or quitting right after
  saving waits for the save instead of asking again; edits made while saving stay unsaved.
- Copy image and Set as wallpaper no longer freeze the window while the picture is read, and say when they are done
  or what went wrong (Set as wallpaper used to fail silently).

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
