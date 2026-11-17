# Lumen roadmap research (after v1.3.0)

Research and planning only. Nothing here is built yet. Written 1 Nov 2026.

## Where Lumen stands

Lumen is already a strong viewer and a good tool for one comparison at a time: 2 to 4 images or videos, mark-up,
comments, PowerPoint/PDF/HTML export, about 25 metrics, problem boxes, AI review and OCR.

What it is missing is the way an image quality team actually works. A team does not compare one set of images.
It compares many scenes, from several devices, over many tuning rounds, and has to say which device wins,
what got better or worse since the last round, and why. Lumen knows nothing about devices, rounds or scenes,
cannot line up photos taken with different fields of view, and builds one report per comparison.

So the step from "very good" to "superb" is turning Lumen from an image viewer into the team's
image quality workbench.

## The ideas, grouped

### A. Projects: Device, Round, Scene (the foundation)
- A project file that knows devices (A56, competitor X), rounds (R1, R2, build numbers) and scenes (night 01, portrait 03).
- Auto pairing from folder names, file name patterns, or EXIF model and capture time. Open a project and get every
  scene already grouped with all its devices.
- Next / previous scene inside Compare, so a whole round can be reviewed without reselecting files.
- Device names (not A/B/C/D) on panes, in metrics and in reports.
- A completeness check: which device, round or scene cells are missing or duplicated.
- Reviews saved inside the project, so they survive moving folders and can be shared on a network drive.
  Today they are keyed on a hash of the file paths and break when files move.

### B. True field-of-view matching (image alignment)
- Today "Match framing" only scales. Phones have different lenses and crops, so the same snip lands on different
  scene content per device, and cross-device metrics compare different pixels.
- Align every image to the reference: feature matching plus a homography, with local refinement.
  Options: ORB (no model, fallback), XFeat (fast on CPU), ALIKED + LightGlue (best, ONNX, Apache/BSD licenses).
- Unlocks: same-content crops on every device, fair full-reference metrics across devices, and a chart-free
  texture metric (texture kept by device X versus the reference on the same crop).
- Also: normalize to the lowest common resolution for fair blind tests.

### C. Structured verdicts
- Attribute scorecard per scene and device, using the industry attribute list (exposure, color, texture, noise,
  artifacts, focus, flash, zoom, bokeh, stabilization). Scale configurable: 1 to 5, or -3 to +3 against a reference.
- Artifact tags on marks from a fixed list (ghosting, halo, flare, moire, ringing, color fringing, banding,
  demosaic errors, judder, jello, flicker) with a severity. Tags can be counted, searched and summarized.
- A golden reference per scene (target device or approved build) and a Win / Par / Lose verdict against it.

### D. Reports that match how teams present
- One deck for a whole round: per scene a full view row, matching 100% crops, scorecard, verdict and marked issues.
- An executive summary slide: win/par/lose tally, attribute radar per device, top issues.
- Company PowerPoint template support (use their master slides and fonts).
- AI findings and problem boxes can be turned into marks with one click, and appear in the report.
- Excel / CSV export of all scores and metrics per device, round and scene.

### E. Rounds and regressions
- Version stack for a scene: previous round, this round, competitor. Score change badges and a regression flag.
- Issues carry an ID and status (open, fixed, won't fix) and are rechecked in the next round's matching scene.
- Export an issue as a ticket draft (JIRA style text plus the cropped evidence).
- Dashboard: scene by device heat map, attribute radar, trend across rounds.

### F. Better metrics
- Region aware metrics: face and skin (skin tone, texture on faces), sky and flat areas (noise where it shows),
  neutral areas (white balance). Small Apache models (MediaPipe segmenter, YuNet face detector).
- Industry style measures that work on real scenes: visual noise (CPIQ style, in Lab), chroma noise, lateral
  chromatic aberration on strong edges, white balance on neutrals, clipping per channel with percentages.
- Express gaps in JND (just noticeable difference) so "B is 2 JND noisier" replaces raw numbers.
- Later: automatic chart detection for lab shots (ColorChecker color error, ISO 12233 sharpness, dead leaves texture).
- Pass / fail limits per metric and scene for regression gates.

### G. Video
- Automatic time alignment of clips from different phones (picture or audio match), with fine shift buttons.
- Per frame metric timeline across all videos: click a dip to jump to that frame.
- Timing checks: dropped and duplicated frames, real frame rate, timestamp jitter.
- Exposure and white balance convergence over time, stabilization jitter, temporal noise.
- VMAF (phone model, NEG mode) when one device is chosen as the reference.
- Share of frames per quality band as a compact report chart.

### H. AI
- Rank 3 or 4 devices in one request (today AI takes 1 or 2 images).
- Recommend proven quality models for the user's vLLM server (Q-Insight, VisualQuality-R1; both Qwen2.5-VL 7B
  fine-tunes) and send aligned, matching crops for fair A vs B judgements. Licenses to be checked.
- AI suggests artifact tags and regions; the engineer confirms or rejects.
- Optional small local score model (TOPIQ or CLIP-IQA+, about 100 to 150 MB) as a tile heat map, only if the
  weight licenses allow company use. Most published weights are non-commercial.

### I. Blind tests and team voting
- Blind mode: hide names and EXIF, random left/right, forced choice, hidden repeat trials.
- Turn many quick votes into a ranking with confidence (Bradley-Terry or Thurstone, as in pwcmp).
- Merge votes from several engineers, show agreement, flag unreliable raters.

### J. HDR
- Ultra HDR (gain map) and Apple HDR photos shown correctly, with an SDR / HDR toggle.
- HDR10 / HLG video tone mapped correctly on SDR screens, real HDR on HDR screens.

### K. Engineering quality
- CI that also tests video (FFmpeg) and AI code paths.
- An automated UI test suite built on the dev hook.
- Compare shortcuts in the remappable keymap.

## Suggested order

| Release | Theme | Contents |
|---|---|---|
| 1.4 | Workbench | A (projects, pairing, next scene, device names), C (scorecard, artifact tags), D (round deck, summary slide, Excel export, AI to marks) |
| 1.5 | Truly matched | B (alignment), F (region metrics, visual noise, JND), H (3-4 device AI ranking, tag suggestions) |
| 1.6 | Rounds | E (version stack, issue status, dashboard), I (blind tests, voting) |
| 1.7 | Video and HDR | G (time alignment, timing checks, convergence), J (HDR), chart detection |

K runs alongside every release.

## Not recommended
- Flicker / blink compare: research says it is the fastest way to spot small differences, but the team asked
  for it to be removed. Leave it out unless they change their mind.
- Bundling pyiqa or most published quality model weights: non-commercial licenses.
- Apple AVQT: macOS only.

## Questions to settle before planning 1.4
1. How are captures organized today (folder layout and file names)? This decides auto pairing.
2. Do several engineers need to work on the same project at once, or is a project file on a shared drive enough?
3. Is there a company PowerPoint template the reports must use?
4. Does the team shoot lab charts too, or only real scenes?
5. Which model runs on the AI server, and do the laptops have a GPU for local models?
6. Is a JIRA (or other tracker) link needed, and which tracker?

## Sources (main)
- DXOMARK camera protocol: https://www.dxomark.com/dxomark-camera-test-protocol/
- IEEE 1858 CPIQ: https://standards.ieee.org/ieee/1858/6931
- ITU-T P.910 subjective methods: https://www.itu.int/rec/T-REC-P.910
- pwcmp pairwise scaling: https://github.com/mantiuk/pwcmp
- Ultra HDR format: https://developer.android.com/media/platform/hdr-image-format
- LightGlue ONNX: https://github.com/fabio-sim/LightGlue-ONNX
- DOVER video quality: https://github.com/vqassessment/DOVER
- Netflix VMAF: https://github.com/Netflix/vmaf
- MSU VQMT: https://videoprocessing.ai/vqmt/
- pixop video-compare: https://github.com/pixop/video-compare
- Imatest pass/fail: https://www.imatest.com/docs/passfail/
- Q-Insight: https://arxiv.org/abs/2503.22679
- pyiqa license note: https://github.com/chaofengc/IQA-PyTorch
