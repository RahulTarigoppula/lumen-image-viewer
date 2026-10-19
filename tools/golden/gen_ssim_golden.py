"""Generate golden vectors for lumen-analysis SSIM / MS-SSIM / PSNR tests.

Dev only (not shipped). Usage: python tools/golden/gen_ssim_golden.py
Requires numpy, scipy and scikit-image (pip install --user scikit-image).

Writes crates/lumen-analysis/tests/golden/*.pgm (8-bit gray, deterministic
synthetic images and distortions) and golden.json with:
- ssim: skimage.metrics.structural_similarity(gaussian_weights=True,
  sigma=1.5, use_sample_covariance=False, data_range=255), plus a few
  values of the full SSIM map at interior pixels;
- psnr: skimage.metrics.peak_signal_noise_ratio(data_range=255);
- ms_ssim: an independent numpy implementation of Wang 2003 (5 scales,
  11x11 Gaussian sigma 1.5 on valid windows, 2x2 average downsampling,
  weights 0.0448 0.2856 0.3001 0.2363 0.1333, negatives clamped to 0).
"""
import json
import pathlib

import numpy as np
from scipy import ndimage
from skimage.metrics import peak_signal_noise_ratio, structural_similarity

OUT = pathlib.Path(__file__).resolve().parents[2] / "crates/lumen-analysis/tests/golden"
W, H = 192, 176


def base_image(rng):
    y, x = np.mgrid[0:H, 0:W].astype(np.float64)
    img = 40 + 0.6 * x + 0.3 * y
    img += 25 * np.sin(x / 5.0) * np.cos(y / 7.0)
    # Hard-edged blocks and thin strokes (text-like detail).
    for _ in range(12):
        x0, y0 = rng.integers(0, W - 24), rng.integers(0, H - 24)
        img[y0:y0 + rng.integers(6, 24), x0:x0 + rng.integers(6, 24)] = rng.integers(0, 256)
    for i in range(10):
        img[20 + 14 * i: 22 + 14 * i, 30:160] = 250 if i % 2 else 10
    img += rng.normal(0, 2.0, img.shape)
    return np.clip(np.round(img), 0, 255).astype(np.uint8)


def distortions(a, rng):
    f = a.astype(np.float64)
    out = {}
    out["noise5"] = f + rng.normal(0, 5, f.shape)
    out["noise15"] = f + rng.normal(0, 15, f.shape)
    out["blur1"] = ndimage.gaussian_filter(f, 1.0)
    out["blur2"] = ndimage.gaussian_filter(f, 2.0)
    blocks = f.reshape(H // 8, 8, W // 8, 8).mean(axis=(1, 3))
    out["blocky"] = 0.4 * f + 0.6 * np.kron(blocks, np.ones((8, 8)))
    out["bright"] = f + 12
    out["contrast"] = (f - 128) * 0.7 + 128
    return {k: np.clip(np.round(v), 0, 255).astype(np.uint8) for k, v in out.items()}


def write_pgm(path, img):
    h, w = img.shape
    path.write_bytes(f"P5\n{w} {h}\n255\n".encode() + img.tobytes())


def gauss_kernel():
    x = np.arange(11) - 5
    g = np.exp(-0.5 * x * x / 1.5 ** 2)
    return g / g.sum()


def ssim_valid(a, b, peak):
    g = gauss_kernel()

    def filt(img):
        t = ndimage.correlate1d(img, g, axis=1, mode="constant")[:, 5:-5]
        return ndimage.correlate1d(t, g, axis=0, mode="constant")[5:-5, :]

    mx, my = filt(a), filt(b)
    vx = filt(a * a) - mx * mx
    vy = filt(b * b) - my * my
    vxy = filt(a * b) - mx * my
    c1, c2 = (0.01 * peak) ** 2, (0.03 * peak) ** 2
    cs = (2 * vxy + c2) / (vx + vy + c2)
    l = (2 * mx * my + c1) / (mx * mx + my * my + c1)
    return (l * cs).mean(), cs.mean()


def ms_ssim(a, b):
    w = [0.0448, 0.2856, 0.3001, 0.2363, 0.1333]
    a, b = a.astype(np.float64), b.astype(np.float64)
    terms = []
    for j in range(5):
        s, cs = ssim_valid(a, b, 255.0)
        terms.append(s if j == 4 else cs)
        if j < 4:
            h2, w2 = a.shape[0] // 2, a.shape[1] // 2
            a = a[:2 * h2, :2 * w2].reshape(h2, 2, w2, 2).mean(axis=(1, 3))
            b = b[:2 * h2, :2 * w2].reshape(h2, 2, w2, 2).mean(axis=(1, 3))
    return float(np.prod([max(t, 0.0) ** wj for t, wj in zip(terms, w)]))


def main():
    rng = np.random.default_rng(20261019)
    OUT.mkdir(parents=True, exist_ok=True)
    a = base_image(rng)
    write_pgm(OUT / "ref.pgm", a)
    cases = []
    probes = [(5, 5), (40, 60), (100, 90), (H - 6, W - 6)]
    for name, b in distortions(a, rng).items():
        write_pgm(OUT / f"{name}.pgm", b)
        s, smap = structural_similarity(a, b, gaussian_weights=True, sigma=1.5, use_sample_covariance=False, data_range=255, full=True)
        mine, _ = ssim_valid(a.astype(np.float64), b.astype(np.float64), 255.0)
        assert abs(mine - s) < 1e-9, (name, mine, s)
        cases.append({
            "name": name,
            "file": f"{name}.pgm",
            "ssim": float(s),
            "ssim_map": [[y, x, float(smap[y, x])] for y, x in probes],
            "psnr": float(peak_signal_noise_ratio(a, b, data_range=255)),
            "ms_ssim": ms_ssim(a, b),
        })
    doc = {
        "generator": "tools/golden/gen_ssim_golden.py",
        "reference": "ref.pgm",
        "width": W,
        "height": H,
        "cases": cases,
    }
    (OUT / "golden.json").write_text(json.dumps(doc, indent=1), encoding="utf8")
    print("wrote", len(cases), "cases to", OUT)


if __name__ == "__main__":
    main()
