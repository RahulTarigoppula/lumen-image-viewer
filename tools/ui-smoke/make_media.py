"""Generate deterministic test media for the UI smoke test.

    python tools/ui-smoke/make_media.py <out_dir> [--no-video]

Layout (like a camera test project):

    RoundA/DeviceX/scene_01.jpg ... scene_03.jpg   reference device
    RoundA/DeviceY/scene_0N.jpg                    shifted, scaled, warmer, noisier
    RoundA/DeviceZ/scene_0N.jpg                    softer, brighter, lower JPEG quality
    Flat/                                          single images of different kinds
    Compare2/                                      scene_01 from X and Y (select all, compare)
    Compare3/                                      scene_01 from X, Y and Z
    Video/clip_DeviceX.mp4, clip_DeviceY.mp4       only when an ffmpeg binary is found

Needs Pillow and numpy. Videos use `ffmpeg` from PATH, or the binary shipped
with the `imageio-ffmpeg` pip package; without either they are skipped.
"""

import math
import os
import shutil
import subprocess
import sys

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont
from PIL.TiffImagePlugin import IFDRational

W, H = 1600, 1200
# Scenes are drawn larger than the output so devices can frame them differently.
MW, MH = 1760, 1320


def font(size):
    for name in ("arial.ttf", "DejaVuSans.ttf", "LiberationSans-Regular.ttf"):
        try:
            return ImageFont.truetype(name, size)
        except OSError:
            pass
    return ImageFont.load_default(size=size)


def gradient(w, h, top, bottom):
    t = np.linspace(0.0, 1.0, h)[:, None, None]
    a = np.array(top, dtype=np.float64)[None, None, :]
    b = np.array(bottom, dtype=np.float64)[None, None, :]
    img = a * (1 - t) + b * t
    return Image.fromarray(np.repeat(img, w, axis=1).astype(np.uint8), "RGB")


def color_patches(d, x, y, size, gap):
    colors = [
        (115, 82, 68), (194, 150, 130), (98, 122, 157), (87, 108, 67), (133, 128, 177), (103, 189, 170),
        (214, 126, 44), (80, 91, 166), (193, 90, 99), (94, 60, 108), (157, 188, 64), (224, 163, 46),
        (56, 61, 150), (70, 148, 73), (175, 54, 60), (231, 199, 31), (187, 86, 149), (8, 133, 161),
        (243, 243, 242), (200, 200, 200), (160, 160, 160), (122, 122, 121), (85, 85, 85), (52, 52, 52),
    ]
    for i, c in enumerate(colors):
        cx, cy = x + (i % 6) * (size + gap), y + (i // 6) * (size + gap)
        d.rectangle([cx, cy, cx + size, cy + size], fill=c)


def wedge(d, x, y, w, h, lines):
    """Converging line pairs: fine detail that softens on a blurry device."""
    for i in range(lines):
        t = i / lines
        x0 = x + t * w
        d.line([(x0, y), (x + w / 2 + (x0 - x - w / 2) * 0.15, y + h)], fill=(20, 20, 20), width=2)


def siemens_star(d, cx, cy, r, spokes=36):
    for k in range(spokes):
        a0 = 2 * math.pi * k / spokes
        a1 = 2 * math.pi * (k + 0.5) / spokes
        d.polygon([(cx, cy), (cx + r * math.cos(a0), cy + r * math.sin(a0)), (cx + r * math.cos(a1), cy + r * math.sin(a1))], fill=(10, 10, 10))


def scene_street(rng):
    img = gradient(MW, MH, (96, 150, 220), (230, 236, 245))
    d = ImageDraw.Draw(img)
    d.rectangle([0, MH * 0.68, MW, MH], fill=(92, 96, 90))
    for i in range(0, MW, 90):
        d.rectangle([i, MH * 0.82, i + 50, MH * 0.83], fill=(230, 230, 220))
    x = 60
    while x < MW - 120:
        bw = int(rng.integers(150, 260))
        bh = int(rng.integers(300, 700))
        top = int(MH * 0.68) - bh
        base = tuple(int(v) for v in rng.integers(90, 200, 3))
        d.rectangle([x, top, x + bw, MH * 0.68], fill=base, outline=(40, 40, 40), width=3)
        for wy in range(top + 20, int(MH * 0.68) - 40, 46):
            for wx in range(x + 16, x + bw - 30, 38):
                lit = rng.random() < 0.35
                d.rectangle([wx, wy, wx + 22, wy + 28], fill=(250, 226, 140) if lit else (60, 70, 86))
        x += bw + int(rng.integers(10, 40))
    d.rounded_rectangle([560, 300, 1260, 520], radius=24, fill=(250, 250, 245), outline=(30, 30, 30), width=6)
    d.text((600, 330), "LUMEN SMOKE TEST", font=font(64), fill=(200, 30, 40))
    d.text((600, 420), "Scene 01  street  ISO 100", font=font(40), fill=(20, 20, 20))
    color_patches(d, 1240, 960, 52, 8)
    wedge(d, 120, 960, 420, 280, 40)
    d.text((600, 1180), "fine detail 1 2 3 4 5 6 7 8 9 10", font=font(18), fill=(250, 250, 250))
    return img


def scene_chart(rng):
    img = Image.new("RGB", (MW, MH), (236, 236, 232))
    d = ImageDraw.Draw(img)
    siemens_star(d, 440, 420, 300)
    for i in range(16):
        v = int(i * 255 / 15)
        d.rectangle([820 + i * 52, 120, 820 + (i + 1) * 52, 260], fill=(v, v, v))
    color_patches(d, 860, 320, 110, 12)
    y = 820
    for size in (12, 16, 20, 28, 36, 48, 64):
        d.text((100, y), f"{size}px  The quick brown fox 0123456789", font=font(size), fill=(15, 15, 15))
        y += size + 14
    for k in range(40):
        x0 = 1100 + k * 14
        d.line([(x0, 900), (x0, 1260)], fill=(0, 0, 0) if k % 2 == 0 else (255, 255, 255), width=1 + k // 14)
    noise = rng.normal(0, 3, (MH, MW, 3))
    return Image.fromarray(np.clip(np.asarray(img, dtype=np.float64) + noise, 0, 255).astype(np.uint8))


def scene_night(rng):
    img = gradient(MW, MH, (8, 10, 28), (30, 26, 40))
    a = np.asarray(img, dtype=np.float64).copy()
    yy, xx = np.mgrid[0:MH, 0:MW]
    for _ in range(14):
        cx, cy = rng.integers(80, MW - 80), rng.integers(MH // 3, MH - 120)
        r = rng.uniform(18, 60)
        col = np.array([255, rng.uniform(150, 230), rng.uniform(60, 160)])
        glow = np.exp(-(((xx - cx) ** 2 + (yy - cy) ** 2) / (2 * (r * 2.5) ** 2)))
        a += glow[:, :, None] * col[None, None, :] * 0.9
    img = Image.fromarray(np.clip(a, 0, 255).astype(np.uint8))
    d = ImageDraw.Draw(img)
    d.text((140, 160), "NIGHT 03  ISO 3200", font=font(72), fill=(220, 220, 200))
    d.rectangle([140, 900, 700, 1180], outline=(180, 180, 160), width=4)
    d.text((170, 940), "low light detail", font=font(40), fill=(150, 150, 140))
    a = np.asarray(img, dtype=np.float64)
    a += rng.normal(0, 14, a.shape)
    return Image.fromarray(np.clip(a, 0, 255).astype(np.uint8))


SCENES = [("scene_01", scene_street), ("scene_02", scene_chart), ("scene_03", scene_night)]

# name, make, model, scale, (dx, dy), wb gain (r, g, b), noise sigma, blur radius, exposure, jpeg quality, iso
DEVICES = [
    ("DeviceX", "Lumen Labs", "Reference X1", 1.00, (0, 0), (1.00, 1.00, 1.00), 1.0, 0.0, 1.00, 94, 100),
    ("DeviceY", "Lumen Labs", "Phone Y", 1.04, (26, -14), (1.06, 1.00, 0.93), 4.5, 0.0, 0.97, 90, 200),
    ("DeviceZ", "Other Corp", "Phone Z Pro", 0.98, (-18, 11), (0.97, 1.00, 1.04), 2.0, 1.3, 1.12, 80, 125),
]


def frame(master, scale, shift):
    """Crop W x H from the master, zoomed by `scale` and shifted by `shift` pixels."""
    s = 1.0 / scale
    cx, cy = MW / 2 + shift[0], MH / 2 + shift[1]
    data = (s, 0, cx - s * W / 2, 0, s, cy - s * H / 2)
    return master.transform((W, H), Image.Transform.AFFINE, data, resample=Image.Resampling.BICUBIC)


def develop(img, dev, rng):
    _, _, _, scale, shift, wb, sigma, blur, expo, _, _ = dev
    img = frame(img, scale, shift)
    if blur > 0:
        img = img.filter(ImageFilter.GaussianBlur(blur))
    a = np.asarray(img, dtype=np.float64) * np.array(wb)[None, None, :] * expo
    a += rng.normal(0, sigma, a.shape)
    return Image.fromarray(np.clip(a, 0, 255).astype(np.uint8))


def exif_for(dev, scene_idx):
    name, make, model, _, _, _, _, _, _, _, iso = dev
    ex = Image.Exif()
    ex[0x010F] = make
    ex[0x0110] = model
    ex[0x0131] = "make_media.py"
    stamp = f"2026:11:0{scene_idx + 1} 10:{20 + scene_idx:02d}:00"
    ex[0x0132] = stamp
    sub = ex.get_ifd(0x8769)
    sub[0x9003] = stamp
    sub[0x8827] = iso * (16 if scene_idx == 2 else 1)
    sub[0x829A] = IFDRational(1, 120 if scene_idx < 2 else 15)
    sub[0x829D] = IFDRational(18, 10)
    sub[0x920A] = IFDRational(54, 10)
    return ex


def save_jpeg(img, path, quality, exif=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    kw = {"quality": quality, "subsampling": 0 if quality >= 90 else 2}
    if exif is not None:
        kw["exif"] = exif
    img.save(path, "JPEG", **kw)


def make_photos(out):
    for si, (scene, fn) in enumerate(SCENES):
        master = fn(np.random.default_rng(100 + si))
        for di, dev in enumerate(DEVICES):
            img = develop(master, dev, np.random.default_rng(1000 + si * 10 + di))
            save_jpeg(img, os.path.join(out, "RoundA", dev[0], f"{scene}.jpg"), dev[9], exif_for(dev, si))
    for n, devs in (("Compare2", DEVICES[:2]), ("Compare3", DEVICES[:3])):
        os.makedirs(os.path.join(out, n), exist_ok=True)
        for dev in devs:
            shutil.copyfile(os.path.join(out, "RoundA", dev[0], "scene_01.jpg"), os.path.join(out, n, f"scene_01_{dev[0]}.jpg"))


def make_flat(out):
    flat = os.path.join(out, "Flat")
    os.makedirs(flat, exist_ok=True)
    rng = np.random.default_rng(7)
    # Smooth 16:9 gradient (banding shows up here).
    x = np.linspace(0, 1, 1920)[None, :]
    y = np.linspace(0, 1, 1080)[:, None]
    g = np.stack([x * 255 + 0 * y, y * 255 + 0 * x, (1 - x) * (1 - y) * 255], axis=-1)
    Image.fromarray(g.astype(np.uint8)).save(os.path.join(flat, "gradient_16x9.png"))
    # Very noisy mid-grey frame.
    n = np.clip(128 + rng.normal(0, 40, (1200, 1600, 3)), 0, 255).astype(np.uint8)
    save_jpeg(Image.fromarray(n), os.path.join(flat, "noise_iso6400.jpg"), 90)
    # Text card.
    card = Image.new("RGB", (1400, 1000), (250, 250, 248))
    d = ImageDraw.Draw(card)
    yy = 40
    for size in (10, 14, 18, 24, 32, 44, 60, 80):
        d.text((40, yy), f"Text {size}px  Hamburgefonstiv 1234567890", font=font(size), fill=(10, 10, 10))
        yy += size + 24
    card.save(os.path.join(flat, "text_card.png"))
    # Tall portrait photo.
    p = scene_street(np.random.default_rng(11)).crop((500, 0, 1400, 1320)).resize((900, 1320))
    save_jpeg(p, os.path.join(flat, "portrait_tall.jpg"), 88)
    # Small icon with transparency.
    ic = Image.new("RGBA", (96, 96), (0, 0, 0, 0))
    d = ImageDraw.Draw(ic)
    d.ellipse([6, 6, 90, 90], fill=(240, 160, 30, 255), outline=(60, 40, 10, 255), width=4)
    d.text((30, 30), "L", font=font(40), fill=(255, 255, 255, 255))
    ic.save(os.path.join(flat, "icon_alpha.png"))


def find_ffmpeg():
    exe = shutil.which("ffmpeg")
    if exe:
        return exe
    try:
        import imageio_ffmpeg

        return imageio_ffmpeg.get_ffmpeg_exe()
    except Exception:
        return None


def make_videos(out):
    ff = find_ffmpeg()
    if not ff:
        print("make_media: no ffmpeg found, skipping videos")
        return False
    vdir = os.path.join(out, "Video")
    os.makedirs(vdir, exist_ok=True)
    src = "testsrc2=size=640x360:rate=30:duration=4"
    variants = [
        ("clip_DeviceX.mp4", "null"),
        ("clip_DeviceY.mp4", "scale=672:378,crop=640:360:22:8,eq=brightness=0.04:saturation=1.2,noise=alls=10:allf=t"),
    ]
    for name, vf in variants:
        dst = os.path.join(vdir, name)
        base = [ff, "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", src, "-vf", vf, "-an", "-threads", "1"]
        for codec in (["-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p", "-g", "30"], ["-c:v", "mpeg4", "-q:v", "4", "-g", "30"]):
            r = subprocess.run(base + codec + ["-fflags", "+bitexact", dst])
            if r.returncode == 0 and os.path.exists(dst) and os.path.getsize(dst) > 0:
                break
        else:
            print(f"make_media: could not encode {name}")
            return False
    return True


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 1:
        print(__doc__)
        sys.exit(2)
    out = os.path.abspath(args[0])
    if os.path.exists(out):
        shutil.rmtree(out)
    os.makedirs(out)
    make_photos(out)
    make_flat(out)
    videos = False if "--no-video" in sys.argv else make_videos(out)
    count = sum(len(f) for _, _, f in os.walk(out))
    print(f"make_media: {count} files in {out} (videos: {'yes' if videos else 'no'})")


if __name__ == "__main__":
    main()
