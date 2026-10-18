"""Generates assets/lumen-256.png and assets/lumen.ico (run: python tools/make_icon.py)."""
from PIL import Image, ImageDraw, ImageFilter
import math, os

S = 1024
here = os.path.dirname(os.path.abspath(__file__))
out = os.path.join(here, "..", "assets")

def lerp(a, b, t): return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))

bg = Image.new("RGBA", (S, S))
top, bot = (64, 132, 255), (118, 72, 236)
px = bg.load()
for y in range(S):
    for x in range(S):
        t = (x * 0.35 + y * 0.65) / S
        px[x, y] = lerp(top, bot, min(1, t)) + (255,)
mask = Image.new("L", (S, S), 0)
ImageDraw.Draw(mask).rounded_rectangle([40, 40, S - 40, S - 40], radius=230, fill=255)
icon = Image.new("RGBA", (S, S), (0, 0, 0, 0))
icon.paste(bg, (0, 0), mask)

d = ImageDraw.Draw(icon)
# Mountains
d.polygon([(170, 790), (420, 470), (560, 640), (650, 540), (860, 790)], fill=(255, 255, 255, 235))
d.polygon([(560, 640), (650, 540), (860, 790), (700, 790)], fill=(255, 255, 255, 200))
# Sun with glow
glow = Image.new("RGBA", (S, S), (0, 0, 0, 0))
ImageDraw.Draw(glow).ellipse([590, 170, 830, 410], fill=(255, 226, 120, 200))
glow = glow.filter(ImageFilter.GaussianBlur(40))
icon = Image.alpha_composite(icon, Image.composite(glow, Image.new("RGBA", (S, S), (0,0,0,0)), mask))
d = ImageDraw.Draw(icon)
d.ellipse([630, 210, 790, 370], fill=(255, 236, 160, 255))
# Play triangle hint (video mode) in the sun-free corner
d.rounded_rectangle([170, 200, 430, 380], radius=40, outline=(255, 255, 255, 230), width=26)
d.polygon([(275, 245), (275, 335), (345, 290)], fill=(255, 255, 255, 235))

icon.resize((256, 256), Image.LANCZOS).save(os.path.join(out, "lumen-256.png"))
icon.save(os.path.join(out, "lumen.ico"), sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print("ok")
