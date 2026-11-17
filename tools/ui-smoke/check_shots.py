"""Check UI smoke screenshots and build an overview.

    python tools/ui-smoke/check_shots.py <out_dir>

Reads <out_dir>/results.json (written by run.ps1), flags screenshots that are
missing or blank (a single flat color: nothing rendered), and writes
index.html (all screenshots by scenario), overview.png (a contact sheet) and,
on GitHub Actions, a table in the job summary. Exits 1 when a screenshot is
missing or blank.
"""

import html
import json
import os
import sys

from PIL import Image, ImageDraw, ImageStat


def stats(path):
    with Image.open(path) as im:
        im = im.convert("RGB")
        w, h = im.size
        small = im.resize((max(1, w // 4), max(1, h // 4)))
        std = sum(ImageStat.Stat(small.convert("L")).stddev)
        colors = small.getcolors(1 << 20)
        return w, h, std, len(colors) if colors else 1 << 20


def main():
    out = sys.argv[1]
    with open(os.path.join(out, "results.json"), encoding="utf-8-sig") as f:
        results = json.load(f)
    if isinstance(results, dict):
        results = [results]
    shots_dir = os.path.join(out, "shots")
    bad = []
    rows = []
    for r in results:
        r["errors"] = r.get("errors") or []
        shots = r.get("shots") or []
        if isinstance(shots, str):
            shots = [shots]
        r["shots"] = shots
        r["shot_info"] = []
        for s in shots:
            p = os.path.join(shots_dir, s)
            if not os.path.exists(p):
                r["shot_info"].append((s, None))
                continue
            w, h, std, ncol = stats(p)
            blank = std < 2.0 or ncol < 4
            if blank and r["status"] == "known-fail":
                r["errors"].append(f"blank screenshot {s}")
            elif blank:
                bad.append(f"{s}: blank ({w}x{h}, stddev {std:.1f}, {ncol} colors)")
                r["errors"].append(f"blank screenshot {s}")
                r["status"] = "fail"
            r["shot_info"].append((s, (w, h, std, blank)))
            rows.append((r["name"], s, p))

    # index.html
    parts = ["<!doctype html><meta charset=utf-8><title>Lumen UI smoke</title>",
             "<style>body{font:14px system-ui;background:#1b1b1f;color:#ddd;margin:16px}"
             "img{max-width:640px;border:1px solid #444;display:block}figure{display:inline-block;margin:8px;vertical-align:top}"
             ".fail,.known-fail{color:#ff6b6b}.pass{color:#6bd68a}.skipped{color:#aaa}</style>",
             "<h1>Lumen UI smoke test</h1>"]
    for r in results:
        parts.append(f"<h2>{html.escape(r['name'])} <span class={r['status']}>{r['status']}</span> <small>{r.get('seconds', '')} s</small></h2>")
        if r.get("known"):
            parts.append(f"<div>Known failure: {html.escape(r['known'])}</div>")
        for e in r["errors"]:
            parts.append(f"<div class=fail>{html.escape(str(e))}</div>")
        parts.append(f"<div><a href='logs/{html.escape(r['name'])}/'>logs</a></div>")
        for s, info in r["shot_info"]:
            cap = html.escape(s) + ("" if info else " (missing)")
            if info:
                cap += f" {info[0]}x{info[1]}"
                parts.append(f"<figure><a href='shots/{html.escape(s)}'><img src='shots/{html.escape(s)}'></a><figcaption>{cap}</figcaption></figure>")
            else:
                parts.append(f"<figure class=fail>{cap}</figure>")
    with open(os.path.join(out, "index.html"), "w", encoding="utf-8") as f:
        f.write("\n".join(parts))

    # overview.png: 3 columns of 480 px thumbnails with captions.
    if rows:
        tw, th, cap, cols = 480, 300, 18, 3
        n = len(rows)
        sheet = Image.new("RGB", (cols * tw, ((n + cols - 1) // cols) * (th + cap)), (24, 24, 28))
        d = ImageDraw.Draw(sheet)
        for i, (_, s, p) in enumerate(rows):
            with Image.open(p) as im:
                im = im.convert("RGB")
                im.thumbnail((tw - 4, th - 4))
                x, y = (i % cols) * tw, (i // cols) * (th + cap)
                sheet.paste(im, (x + 2, y + cap))
                d.text((x + 4, y + 3), s[:70], fill=(230, 230, 230))
        sheet.save(os.path.join(out, "overview.png"))

    # Job summary on GitHub Actions.
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a", encoding="utf-8") as f:
            f.write("## UI smoke test\n\n| Scenario | Result | Time | Screenshots | Problems |\n|---|---|---|---|---|\n")
            for r in results:
                probs = "; ".join(str(e) for e in r["errors"]).replace("|", "/")[:300]
                if r.get("known"):
                    probs = f"known: {r['known']}. " + probs
                f.write(f"| {r['name']} | {r['status']} | {r.get('seconds', '')} s | {len(r['shots'])} | {probs} |\n")
            f.write("\nScreenshots and logs: artifact `ui-smoke` (open index.html or overview.png).\n")

    for r in results:
        for s, info in r["shot_info"]:
            if info is None and r["status"] not in ("skipped", "known-fail"):
                bad.append(f"{s}: missing")
    for b in bad:
        print(f"check_shots: {b}")
    print(f"check_shots: {len(rows)} screenshots checked, {len(bad)} problems")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
