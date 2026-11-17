# Lumen 2.0 plan: from viewer to image quality workbench

Written 17 Nov 2026. Builds on [ROADMAP-RESEARCH.md](ROADMAP-RESEARCH.md); the open questions there are
answered with defaults that work for most teams and can be changed later.

## Defaults chosen for the open questions

| Question | Default |
|---|---|
| How captures are organized | Detected automatically: one folder per device (`Round1/GalaxyA56/night_01.jpg`), or device and scene in the file name (`night_01_A56.jpg`, `A56-night_01.jpg`), or EXIF camera model plus capture time. The user can fix the result. |
| Several engineers at once | A project file (`lumen-project.json`) beside the captures; it stores paths relative to itself, so it works on a network drive and survives moving the folder. Last writer wins, with a reload prompt when it changed on disk. |
| Company template | Built-in clean template; colors and logo configurable later. |
| Lab charts | Real scenes first; chart detection later. |
| AI model | Any OpenAI-compatible vision model (as today). |
| Tracker | Plain text and CSV export; no tracker integration. |

## Workstreams

| # | Theme | Contents | Where |
|---|---|---|---|
| 1 | Workbench (projects) | Device / scene detection, project file, completeness grid, open a scene in Compare, next / previous scene, device names on panes | `lumen-core::workbench`, `ui/workbench.rs`, compare |
| 2 | Structured verdicts | Attribute scorecard per scene and device, Win / Par / Lose against a reference device, artifact tags with severity on marks | `lumen-core::workbench`, review panel |
| 3 | Round report | One deck for a whole project: summary slide with tallies and per-device attribute bars, then every scene; CSV of scores, tags and metrics | `lumen-core::report`, `ui/review.rs` |
| 4 | True field-of-view matching | Feature matching (FAST + rotated BRIEF), RANSAC homography, warp to the reference: same-content crops and fair metrics across phones | `lumen-analysis::align`, compare |
| 5 | Camera metrics | Visual noise (Lab), chroma noise, white balance on neutral areas, clipping per channel, differences expressed in JND | `lumen-analysis::metrics::noref` |
| 6 | Video timing | Real frame rate, dropped and duplicated frames, timestamp jitter; automatic start point alignment between clips from different phones | `lumen-core::video::timing`, video mode |
| 7 | Blind test | Forced-choice A/B trials over a project with hidden names and random sides; Bradley-Terry ranking with confidence | `lumen-core::workbench::blind`, compare |
| 8 | Engineering | CI that builds video and AI, UI smoke tests with screenshots, remappable compare keys, performance passes | `.github`, `tools/ui-smoke` |

## Order

1. CI dev checks (done first: every change compiles and is tested on Windows with FFmpeg).
2. Workstreams 4, 5, 6 and 8 run in parallel (pure logic first, each with unit tests).
3. Workstreams 1, 2, 3 (the workbench) form the core of 2.0.
4. Workstream 7 and integration of alignment into Compare and reports.
5. Performance pass, documentation, version 2.0.0.
