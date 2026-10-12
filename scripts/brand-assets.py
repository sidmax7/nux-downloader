#!/usr/bin/env python3
"""Builds every logo asset from assets/brand/logo-master.png.

The master is the full logo (mark plus "NuxDownloader" wordmark) on the
brand's dark navy. This cuts the mark and the full logo out with a real
alpha matte (distance from the background colour, then un-premultiplied,
so edges carry no dark fringe), and writes:

  assets/brand/mark.png                 the mark, transparent
  assets/brand/logo-transparent.png     mark + wordmark, transparent
  assets/brand/app-icon.png             1024 px app icon (input to `tauri icon`)
  src-tauri/icons/tray.png              tray icon, the mark alone
  ui/src/assets/mark.png                the mark for the app's sidebar
  ui/public/favicon.png                 favicon
  docs/assets/logo.png                  README header

Then regenerate the platform icons with:

  cd src-tauri && ../ui/node_modules/.bin/tauri icon ../assets/brand/app-icon.png

Needs Pillow and numpy. Run from the repository root.
"""

import numpy as np
from PIL import Image, ImageDraw

MASTER = "assets/brand/logo-master.png"
BG = np.array([9.7, 9.6, 17.55]) / 255  # the master's background


def matte(region: np.ndarray) -> Image.Image:
    distance = np.abs(region - BG).max(axis=2)
    alpha = np.clip((distance - 0.025) / 0.20, 0, 1)
    rgb = np.clip((region - BG * (1 - alpha[..., None])) / np.maximum(alpha, 1e-4)[..., None], 0, 1)
    image = Image.fromarray((np.dstack([rgb, alpha]) * 255 + 0.5).astype(np.uint8), "RGBA")
    return image.crop(image.getbbox())


def bbox_of_bright(region: np.ndarray, y0: int, x0: int = 0) -> tuple[int, int, int, int]:
    """Top, bottom, left, right of everything that isn't background."""
    ys, xs = np.nonzero(np.abs(region - BG).max(axis=2) > 0.12)
    return y0 + ys.min(), y0 + ys.max() + 1, x0 + xs.min(), x0 + xs.max() + 1


def app_icon(mark: Image.Image, size: int = 1024) -> Image.Image:
    """The mark on a rounded navy tile with a soft violet glow."""
    tile, radius = int(size * 0.86), int(size * 0.195)
    yy, xx = np.mgrid[0:tile, 0:tile].astype(np.float32)
    r = np.hypot(xx - tile * 0.52, yy - tile * 0.46) / (tile * 0.62)
    glow = np.clip(1 - r, 0, 1) ** 1.6
    base, high = np.array([10, 10, 18], np.float32), np.array([40, 26, 92], np.float32)
    background = Image.fromarray((base + (high - base) * glow[..., None]).astype(np.uint8)).convert("RGBA")
    mask = Image.new("L", (tile, tile), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, tile - 1, tile - 1], radius=radius, fill=255)
    background.putalpha(mask)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(background, ((size - tile) // 2, (size - tile) // 2))
    w = int(tile * 0.78)
    h = round(mark.height * w / mark.width)
    canvas.alpha_composite(mark.resize((w, h), Image.LANCZOS), ((size - w) // 2 + 6, (size - h) // 2 - 4))
    return canvas


def square(mark: Image.Image, size: int, fill: float) -> Image.Image:
    w = int(size * fill)
    h = round(mark.height * w / mark.width)
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.alpha_composite(mark.resize((w, h), Image.LANCZOS), ((size - w) // 2, (size - h) // 2))
    return canvas


def main() -> None:
    master = Image.open(MASTER).convert("RGB")
    pixels = np.asarray(master).astype(np.float32) / 255
    height = pixels.shape[0]

    # The mark sits above the wordmark; split at the widest empty band.
    rows = np.abs(pixels - BG).max(axis=2).max(axis=1) > 0.12
    filled = np.nonzero(rows)[0]
    gaps = [(b - a, a, b) for a, b in zip(filled, filled[1:]) if b - a > 1]
    split = max(gaps)[1] + max(gaps)[0] // 2 if gaps else height // 2

    pad = 8
    t, b, l, r = bbox_of_bright(pixels[:split], 0)
    mark = matte(pixels[max(t - pad, 0) : b + pad, max(l - pad, 0) : r + pad])
    t2, b2, l2, r2 = bbox_of_bright(pixels, 0)
    full = matte(pixels[max(t2 - pad, 0) : b2 + pad, max(l2 - pad, 0) : r2 + pad])

    mark.save("assets/brand/mark.png")
    full.save("assets/brand/logo-transparent.png")
    icon = app_icon(mark)
    icon.save("assets/brand/app-icon.png")
    square(mark, 256, 0.96).resize((64, 64), Image.LANCZOS).save("src-tauri/icons/tray.png")
    mark.resize((round(mark.width * 72 / mark.height), 72), Image.LANCZOS).save("ui/src/assets/mark.png")
    icon.resize((64, 64), Image.LANCZOS).save("ui/public/favicon.png")
    # README: the master as designed, on its own navy, at a web size.
    side = 520
    master.resize((side, side), Image.LANCZOS).crop((0, int(side * 0.2), side, int(side * 0.76))).save(
        "docs/assets/logo.png", optimize=True
    )


if __name__ == "__main__":
    main()
