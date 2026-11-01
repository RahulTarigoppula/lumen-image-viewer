# Text and document analysis

Goal: when the picture is a document, a receipt, a whiteboard, a screenshot or
any image with text, Lumen reads the text and tells the user, in plain words,
whether the capture is good enough to read and where it fails. Comparing 2-4
captures of the same page (different scanning apps, phones, settings) shows
which one reads best and which words each one lost.

Everything runs on this computer. The OCR engine is the one built into
Windows (`Windows.Media.Ocr`); nothing is downloaded or sent anywhere.

## 1. What document-scan quality checks measure

Scanning apps, archive digitization guidelines (FADGI, ISO 19264-1, Metamorfoze)
and OCR pipelines (Tesseract, ABBYY, cloud OCR services) look at roughly the
same list. The table says how each is usually computed, how Lumen computes it,
and in which phase.

| Check | What it tells the user | Usual measurement | Lumen | Phase |
|---|---|---|---|---|
| OCR detection / recognition rate | Can a machine read the page at all? How much text was found? | Words and characters recognized; engine confidence per word; against ground truth: character / word error rate (CER / WER) | Words, characters, lines from Windows OCR. Windows OCR gives no confidence, so a "readable words" share is used: a word counts as readable when it is mostly letters / digits and not a jumble of symbols. Against another capture: word-level diff and similarity | 1 |
| Text size, effective resolution | Is the text big enough in pixels? (300 dpi for 10-12 pt text is the usual archive target; OCR engines want an x-height of ~10-20 px or more) | Pixels per text height, or dpi from metadata times font size | Median word-box height in pixels (word boxes span ascenders to descenders, about the font size). Verdict: 20 px and up good, 12-20 small, under 12 too small. Also "about N dpi for 11 pt text" | 1 |
| Text sharpness (clarity) | Is the text in focus / not motion-blurred? | Edge width or gradient strength at stroke edges; Laplacian variance; MTF / SFR from slanted edges | Per word: stroke contrast (spread of brightness in the box) and the strongest gradients; edge width = contrast / strong gradient, in px, compared with the estimated stroke width (12% of the text height). Gives a 0-100 clarity score; words under 40 are flagged blurry | 1 |
| Stroke contrast / low contrast | Faint, washed-out or grey-on-grey text | Difference between ink and paper luminance; Michelson or Weber contrast | Per word: 97th minus 3rd percentile of luminance. Under 100 of 255 is flagged faint | 1 |
| Clipping / cropped text | Did the crop or the page edge cut words off? | Text boxes touching the image border; ink touching the border | Words whose box lies within a few pixels of an image border are "cut at the left / right / top / bottom edge" | 1 (words); 2 (ink touching the border without a recognized word) |
| Overlapping text, ghosting, double print | Two exposures merged, a page shining through, a stitched panorama seam, a double-fed scan | Overlapping text boxes; autocorrelation peaks of the ink image (shifted copy) | Word boxes that overlap each other by 30% or more of the smaller box | 1 (boxes); 2 (shifted-copy detection from pixels) |
| Skew / rotation | Page tilted in the scan | Hough transform or projection profiles of text lines; OCR engine's text angle | Windows OCR's detected text angle; when missing, the median slope of the word bottoms along each line | 1 |
| Unread text (coverage) | Text the recognizer skipped: smeared, doubled or blurred-away lines that produce no words at all | Share of ink pixels not covered by recognized text boxes | Dark marks (darker than the paper by 40% of the ink-paper range, sampled on a grid) outside every word box grown by a fifth of its height; over 15% is reported, with the area where most of them are. Pictures, stamps and table lines count too | 1 |
| Perspective (keystone) | Phone photo taken at an angle | Page corner detection, line convergence | Difference in skew between top and bottom lines; page quad detection | 2 |
| Uneven lighting, shadows | Dark corners, hand or phone shadow, curl near the spine | Background (paper) brightness map: large-window max filter or morphological closing, then its spread | Paper-brightness map on a coarse grid; spread and darkest region | 2 |
| Binarization quality | Black-and-white modes that eat thin strokes or leave speckle | Otsu threshold separability; broken / filled characters; speckle count | Otsu separability and small-blob count per word | 2 |
| Compression and moire around glyphs | JPEG ringing and blocks around letters, moire from screen photos | Blockiness, ringing energy next to edges; periodic peaks in the spectrum | Ringing energy in a band around strokes; 8x8 block edge energy inside text boxes; FFT peaks | 2 |
| Noise, speckle, bleed-through | Grain in the paper area, back page showing through | Noise sigma in flat paper areas; faint mirrored text | Noise sigma in paper areas between lines | 2 |
| Margins and page completeness | Page fully inside the capture, even margins | Page boundary detection | Text block bounding box versus image; margin per side | 2 |
| Layout / reading order | Columns, tables kept | Layout analysis | Not planned (outside an image viewer's scope) | - |

Notes on the measurements:

- Word boxes come from the OCR engine, so blur and contrast are measured
  where the text actually is, not over the whole page (a page of mostly white
  paper would otherwise look "soft" or "low contrast").
- Edge width is in pixels and is compared with the stroke width, not with a
  fixed number: a 3 px soft edge ruins 12 px text but not 80 px headlines.
- Windows OCR boxes are axis-aligned in the input image's pixels; the engine
  deskews internally and reports the angle it found.
- `OcrEngine::MaxImageDimension` (10,000 px today) limits the input: bigger
  images are scaled down for recognition and the boxes are scaled back.

## 2. Comparing captures

The usual way to compare OCR output is CER / WER against ground truth. With
two scans of the same page there is no ground truth, so image A is the
reference and every other image is diffed against it:

- Tokens are words in reading order (lines top to bottom as the engine
  returns them). Words are matched after normalizing case and stripping
  surrounding punctuation, so "Total:" and "total" count as the same word;
  the original spelling is shown.
- Diff: Myers' O(ND) algorithm on the token lists. A deletion next to an
  insertion is reported as a changed word ("recieve" became "receive").
- Similarity = 2 × matching words / (words in A + words in B), in percent.
- Counts per image: missing (in A, not in this image), extra (only in this
  image), changed.

## 3. What the UI shows

A "Text" tab in the Analysis panel (Ctrl+Q):

- Language picker (the OCR languages installed in Windows; "Automatic" uses
  the user's profile languages) and a **Read text** button. In compare mode it
  reads every pane.
- Per image: a 0-100 document score and one plain sentence, e.g. "312 words
  read, text sharp, 2 words cut at the right edge, page tilted 1.8°." Then
  one row per check (text size, sharpness, contrast, cut words, overlapping
  text, tilt, readable words) with a status dot and a tooltip in plain words.
- Issues: the individual words that are blurry, faint, cut or overlapping.
  Clicking one zooms the image to it.
- Recognized text: selectable, with a Copy button.
- Compare: a table with one row per image (score, words, size, sharp share,
  cut, overlapping, tilt, match with A), then the word diff of A against the
  picked image with missing, extra and changed words colored.
- On the image: word boxes colored by clarity (green / amber / red), cut words
  outlined red, overlapping words purple. A toggle hides them. Only panes on
  screen are drawn and boxes outside the visible area are skipped.
- `TextView::report_table()` gives the per-image table to the review report
  (PowerPoint / PDF / HTML).

When no OCR language is installed, the tab says so and how to add one
(Settings > Time & language > Language & region > add a language with
"Optical character recognition").

## 4. Phases

**Phase 1 (this change)**

- `lumen_core::ocr`: Windows OCR wrapper (`recognize`, `available_languages`)
  on its own MTA thread, scaling to the engine's maximum size.
- `lumen_core::text`: pure analysis (size, clarity, contrast, clipping,
  overlap, skew, readable words, score, summary) and the word diff, with unit
  tests on synthetic boxes and images.
- `src/analysis_ui/text_view.rs`: the Text tab, overlays, zoom to issue,
  copy text, compare table and diff, report table.

**Phase 2**

- Pixel-based checks that do not need recognized words: ink touching the
  border, shifted-copy (ghost) detection, illumination map and shadows,
  binarization and speckle, JPEG ringing / blocking and moire around glyphs,
  perspective.
- Diff highlights on the images (missing words boxed on A, extra on B).
- Region of interest support (read only the selected region).
- Video frames (read the paused frame).
- Text results in the review report and the AI prompt context.

**Phase 3**

- Character error rate against a typed or pasted reference text.
- Batch: read a folder of scans and export a CSV of scores.
