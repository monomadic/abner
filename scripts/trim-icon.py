#!/usr/bin/env python3
"""Crop an icon render to a square, full-bleed, opaque macOS app icon.

Apple's Human Interface Guidelines ("App icons" > Icon shape) state the rule
plainly, and it is the whole rule:

    Produce appropriately shaped, unmasked layers. The system masks all layer
    edges to produce an icon's final shape. For iOS, iPadOS, and macOS icons,
    provide square layers so the system can apply rounded corners. Providing
    layers with pre-defined masking negatively impacts specular highlight
    effects and makes edges look jagged.

    If you do import a background layer, make sure it's full-bleed and opaque.

So: square, full-bleed, opaque, UNMASKED, 1024x1024. macOS 26 applies the
rounded-rectangle mask itself. An icon that does not meet this gets adapted by
the system instead — which is how abner's Dock icon ended up shrunk inside a
lighter plate and looking permanently padded (see HISTORY.md).

The renders in assets/icons/ do not meet it: they arrive as a rounded body
floating in a transparent margin, at ~89% of their canvas. This trims to the
alpha bounding box and scales the artwork up until the 1024x1024 centre crop is
fully opaque corner to corner, so the render's own rounded corners are cropped
away and the system's mask supplies the silhouette.

    python3 scripts/trim-icon.py assets/icons/app-icon-07.png assets/app-icon.png

Run it whenever an alternate is copied over the icon SLOT (assets/app-icon.png).

--zoom defaults to the SMALLEST value that is fully opaque, found by search, so
the crop is never tighter than the guideline requires; pass one to crop further.
The HIG also says to avoid soft, feathered edges and to leave specular
highlights, bevels and glows to the system — worth remembering when picking the
next render, since these all carry their own.
"""
import argparse

from PIL import Image
import numpy as np

CANVAS = 1024
ALPHA_FLOOR = 2     # low, so the render's soft glow isn't mistaken for margin
OPAQUE = 250        # "fully opaque" allowing for resample ringing


def crop_at(art0, zoom):
    scale = CANVAS * zoom / max(art0.size)
    art = art0.resize((round(art0.size[0] * scale), round(art0.size[1] * scale)), Image.LANCZOS)
    ox, oy = (art.size[0] - CANVAS) // 2, (art.size[1] - CANVAS) // 2
    if ox < 0 or oy < 0:
        return None
    return art.crop((ox, oy, ox + CANVAS, oy + CANVAS))


def minimum_zoom(art0):
    lo, hi = 1.0, 2.0
    for _ in range(24):
        mid = (lo + hi) / 2
        out = crop_at(art0, mid)
        if out is not None and np.array(out)[:, :, 3].min() >= OPAQUE:
            hi = mid
        else:
            lo = mid
    # rounding in the resample makes opacity non-monotonic right at the boundary,
    # so step up until the value actually returned survives its own crop
    z = round(hi, 3)
    for _ in range(20):
        out = crop_at(art0, z)
        if out is not None and np.array(out)[:, :, 3].min() >= OPAQUE:
            return z
        z = round(z + 0.005, 3)
    return z


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("src")
    ap.add_argument("dst")
    ap.add_argument("--zoom", type=float, default=None,
                    help="scale past the canvas before cropping; default = smallest fully opaque")
    args = ap.parse_args()

    im = Image.open(args.src).convert("RGBA")
    ys, xs = np.where(np.array(im)[:, :, 3] > ALPHA_FLOOR)
    art0 = im.crop((xs.min(), ys.min(), xs.max() + 1, ys.max() + 1))

    floor = minimum_zoom(art0)
    zoom = args.zoom if args.zoom is not None else floor
    out = crop_at(art0, zoom)
    if out is None:
        raise SystemExit(f"--zoom {zoom} is too small to cover the canvas")

    alpha = np.array(out)[:, :, 3]
    out.convert("RGB").save(args.dst)      # flatten: full-bleed and opaque, no mask

    print(f"{args.src} -> {args.dst} {CANVAS}x{CANVAS} RGB, "
          f"zoom {zoom} (keeps {100/zoom:.0f}% of the artwork)")
    if alpha.min() < OPAQUE:
        print(f"  WARNING: not full-bleed at this zoom (min alpha {alpha.min()}); "
              f"the transparent edge is flattened to white. Smallest opaque zoom is {floor}.")
    else:
        print(f"  square, full-bleed, opaque, unmasked — per HIG (smallest opaque zoom: {floor})")


if __name__ == "__main__":
    main()
