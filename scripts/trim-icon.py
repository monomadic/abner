#!/usr/bin/env python3
"""Fit an icon render to the macOS 26 icon tile.

Tahoe masks the app icon to its own squircle — but ONLY if the icon covers that
squircle. Hand it artwork floating in a margin (which is how every render in
assets/icons/ arrives: a shape at ~89% of its canvas with a transparent
surround) and the system instead drops it on a white plate and shrinks it
inside, a padded undersized tile that no amount of centring or trimming fixes.
The margin itself is the trigger, not the alpha: artwork whose alpha follows the
tile edge composites correctly, which is what this writes.

So: trim to the artwork's alpha bounding box, scale it just past the canvas,
centre-crop to 1024x1024, and mask to a superellipse cut a touch wider than
Apple's own corner (surplus alpha is cut by the system mask; a DEFICIT would
leave slivers of margin and bring the plate back). The corners of the render's
rounded body are the only part lost, and pre-Tahoe still gets a rounded icon.

    python3 scripts/trim-icon.py assets/icons/app-icon-07.png assets/app-icon.png

Run it whenever an alternate is copied over the icon SLOT (assets/app-icon.png).

--zoom is how far past the canvas the artwork is scaled before the crop, so
1/zoom of it survives. The default keeps 95%: measured against the tile shape
macOS composites for a system app, 1.05 is the point where the artwork covers
the mask completely (1.00 leaves 0.9% of it bare, and the plate returns).
Raise it to crop in tighter, never lower it below 1.05.
"""
import argparse

from PIL import Image
import numpy as np

CANVAS = 1024
ALPHA_FLOOR = 2   # low, so the render's soft glow isn't mistaken for margin
POWER = 5         # superellipse exponent; squarer than Apple's corner, deliberately


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--zoom", type=float, default=1.05,
                    help="scale past the canvas before cropping; keeps 1/zoom (default 1.05)")
    args = ap.parse_args()

    im = Image.open(args.src).convert("RGBA")
    ys, xs = np.where(np.array(im)[:, :, 3] > ALPHA_FLOOR)
    art = im.crop((xs.min(), ys.min(), xs.max() + 1, ys.max() + 1))

    scale = CANVAS * args.zoom / max(art.size)
    art = art.resize((round(art.size[0] * scale), round(art.size[1] * scale)), Image.LANCZOS)
    ox, oy = (art.size[0] - CANVAS) // 2, (art.size[1] - CANVAS) // 2
    out = np.array(art.crop((ox, oy, ox + CANVAS, oy + CANVAS)))

    y, x = np.mgrid[0:CANVAS, 0:CANVAS]
    c = (CANVAS - 1) / 2
    d = np.abs((x - c) / c) ** POWER + np.abs((y - c) / c) ** POWER
    mask = np.clip((1.0 - d) * 170 + 0.5, 0, 1)          # antialiased edge

    inside = d < 0.95
    bare = int((inside & (out[:, :, 3] < 250)).sum())

    out[:, :, 3] = np.minimum(out[:, :, 3], (mask * 255).astype("uint8"))
    Image.fromarray(out).save(args.dst)

    print(f"{args.src} -> {args.dst} {CANVAS}x{CANVAS}, zoom {args.zoom} "
          f"(keeps {1/args.zoom:.0%} of the artwork)")
    if bare:
        print(f"  WARNING: {bare} px inside the tile are not opaque — the white plate "
              f"will come back. Raise --zoom.")
    else:
        print("  tile fully covered by the artwork")


if __name__ == "__main__":
    main()
