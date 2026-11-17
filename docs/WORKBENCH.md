# Workbench: camera comparison projects

The Workbench turns a folder of captures from several phones or cameras into a
project you can review scene by scene, score, blind test and report in one go.

## Open a project

- **Tools, Workbench** (Ctrl+Shift+W) treats the folder shown in the browser as
  a project; **Tools, Open camera comparison project...** picks a folder.
- Lumen finds the devices and scenes on its own and saves the result as
  `lumen-project.json` in that folder. Paths in it are relative, so the folder
  can be moved, zipped or shared on a network drive. Reviews (marks and
  comments) are kept beside it in `lumen-reviews/`.

How captures are recognized:

| Layout | Example | Scenes matched by |
|---|---|---|
| One folder per device | `Round3/A56/night_01.jpg`, `Round3/PhoneX/night_01.jpg` | file name (the device name inside a file name is ignored, so `night_01_A56.jpg` and `night_01_X.jpg` match) |
| One folder per device, camera numbering | `A56/IMG_1234.jpg`, `PhoneX/PXL_20261101.jpg` | capture order (capture time when every file has one) |
| One folder, device in the name | `night_01_A56.jpg`, `A56-night_01.jpg`, `night 01 (PhoneX).jpg` | the rest of the name |
| One folder, camera numbering | `IMG_0001.jpg` from two phones | EXIF camera model is the device, capture order the scene |

The side panel lists missing captures (a device without a shot of a scene) and
scenes with extra captures. After adding files, **Setup, Find devices and
scenes again** keeps scores, comments and votes of scenes that still exist.

## Review a scene

Double-click a scene (or press Enter) to open it in Compare:

- Each pane shows the device name and its color.
- The scene bar on top shows the scene and its position; **Page Up / Page
  Down** go to the previous / next scene, **Reviewed** marks it done.
- The **Scorecard** panel (S) holds, per device:
  - **Win / Par / Lose** against the reference device (the star in the
    project's device list);
  - one score per attribute (Exposure, Color, Detail, Noise, Artifacts, Focus,
    Dynamic range, Overall by default), on a 1 to 5 scale or -3 to +3 against
    the reference (Setup);
  - a short note, and a comment for the whole scene.
- Mark up (M) and the Review panel (C) work as in any compare. Each mark can
  carry an **artifact tag** (Halo, Noise, Flare, Banding, White balance, ...)
  and a **severity** (Minor, Moderate, Severe); the round report counts the
  tags per device.

Everything is saved automatically. When someone else saves the same project
(for example on a shared drive) and you have no unsaved changes, Lumen reloads
it.

## Blind test

**Blind test** shows random pairs of captures of the same scene with the
device names, file names and camera data hidden and the sides shuffled. Press
**Left** or **Right** for the better image, **Down** (or Space) when they look
the same, Esc to stop. Pairs with the fewest votes come first, so every pair of
every scene gets judged before any is repeated.

Votes are turned into a ranking with the Bradley-Terry model: the score is the
chance (0 to 100) that a device is preferred over an average device, with a
90% interval from a bootstrap. Overlapping intervals mean the difference is
not certain yet; more votes narrow them. Votes from several people (the
Windows user name is stored with each vote) all count.

## Round report

**Round report** writes one PowerPoint, PDF or web page for the project:

1. Title page with the devices.
2. Executive summary: your conclusion, Win / Par / Lose bars per device and
   mean scores per attribute.
3. Scorecard by attribute: grouped bars per device.
4. Flagged artifacts: how many marks of each tag per device.
5. Blind test ranking with intervals.
6. Every scene: captures side by side with device colors, verdict badges,
   scores, notes, the scene comment and the tagged marks; then its detail
   crops (optional).

**CSV** saves the scores, verdicts and notes per scene and device for a
spreadsheet.
