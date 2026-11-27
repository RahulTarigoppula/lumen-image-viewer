"""Make the lossless JPEG test files (libjpeg-turbo through Pillow).

    python crates/lumen-core/src/jpeg_lossless/testdata/make.py

Pillow's progressive JPEGs use libjpeg's scan script with successive
approximation (refinement scans), which the Rust encoder used in the unit
tests cannot write. Needs Pillow and numpy.
"""

import os

import numpy as np
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))


def scene(w, h, seed):
    rng = np.random.default_rng(seed)
    y, x = np.mgrid[0:h, 0:w].astype(np.float64)
    img = np.stack([x * 255 / max(w - 1, 1), y * 255 / max(h - 1, 1), 128 + 100 * np.sin(x / 5.0) * np.cos(y / 7.0)], -1)
    img[(x - w * 0.3) ** 2 + (y - h * 0.4) ** 2 < (min(w, h) * 0.2) ** 2] = [250, 40, 30]
    img[(x > w * 0.6) & (x < w * 0.8) & (y > h * 0.2) & (y < h * 0.7)] = [20, 30, 200]
    img += rng.normal(0, 12, img.shape)
    return Image.fromarray(np.clip(img, 0, 255).astype(np.uint8))


def save(img, name, **kw):
    img.save(os.path.join(HERE, name), "JPEG", **kw)


save(scene(117, 88, 1), "prog420_rst.jpg", quality=85, subsampling="4:2:0", progressive=True, restart_marker_blocks=7)
save(scene(61, 47, 2), "prog444.jpg", quality=90, subsampling="4:4:4", progressive=True)
save(scene(90, 50, 3), "prog422_rows.jpg", quality=80, subsampling="4:2:2", progressive=True, restart_marker_rows=1)
save(scene(70, 41, 4).convert("L"), "prog_gray_rst.jpg", quality=85, progressive=True, restart_marker_blocks=5)
save(scene(40, 30, 5).convert("CMYK"), "prog_cmyk.jpg", quality=85, progressive=True)
save(scene(77, 51, 6), "base420_rst.jpg", quality=85, subsampling="4:2:0", restart_marker_blocks=3)
