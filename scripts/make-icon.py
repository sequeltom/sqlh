# Draws sqlh's icon and writes assets/sqlh.ico, which build.rs puts in the exe's resources.
# A dev tool, not part of the build: the .ico is committed, so building sqlh needs no Python.
# Needs Pillow. Run from the repository root: python -I scripts/make-icon.py
#
# Each size is drawn at 8x and scaled down, with the strokes widened at the small sizes so
# they stay legible. Every image is stored as PNG, which Windows reads in icons since Vista,
# to keep the exe within its size budget (AGENTS.md, Lean and fast).
import io
import struct
import sys
from pathlib import Path

from PIL import Image, ImageDraw

# Windows' standard set; it scales the nearest for the sizes between.
SIZES = [16, 24, 32, 48, 256]
NAVY = (30, 58, 95, 255)
WHITE = (236, 242, 248, 255)
AMBER = (245, 166, 35, 255)
SCALE = 8


def draw(size):
    s = size * SCALE
    u = s / 32  # one unit of a 32-unit design grid
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    # The tile: a rounded square, inset a little at the large sizes as Windows icons are.
    inset = 0 if size <= 24 else 1 * u
    d.rounded_rectangle([inset, inset, s - inset, s - inset], radius=6 * u, fill=NAVY)

    # The cylinder: a body with an elliptical top and bottom.
    left, right = 8 * u, 24 * u
    top, bottom = 6.5 * u, 25.5 * u
    rim = 3 * u  # the ellipse's half height
    d.ellipse([left, bottom - rim, right, bottom + rim], fill=WHITE)
    d.rectangle([left, top, right, bottom], fill=WHITE)
    d.ellipse([left, top - rim, right, top + rim], fill=WHITE)
    # The top's edge, so the cylinder reads as one at a glance.
    edge = max(1.0, 1.2 * u) if size > 24 else 1.6 * u
    d.arc([left + edge, top - rim + edge * 0.6, right - edge, top + rim - edge * 0.6],
          start=0, end=180, fill=NAVY, width=round(edge))

    # The prompt, `>_`, on the body.
    w = round((2.4 if size <= 24 else 2.0) * u)
    cx, cy = 11.5 * u, 17.5 * u
    d.line([(cx, cy - 3.5 * u), (cx + 3.5 * u, cy), (cx, cy + 3.5 * u)],
           fill=AMBER, width=w, joint="curve")
    d.line([(cx + 5.5 * u, cy + 3.5 * u), (cx + 10 * u, cy + 3.5 * u)], fill=AMBER, width=w)
    return img.resize((size, size), Image.LANCZOS)


def png(img):
    # A palette PNG: four flat colors and their anti-aliased edges fit in 64 entries, at a
    # fraction of the size of full color.
    out = io.BytesIO()
    img.quantize(colors=64, method=Image.Quantize.FASTOCTREE).save(out, "PNG", optimize=True)
    return out.getvalue()


def ico(images):
    # ICONDIR, then an ICONDIRENTRY per image, then the images (Win32 documentation, "Icons").
    blobs = [png(img) for img in images]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for img, blob in zip(images, blobs):
        w = img.width if img.width < 256 else 0
        out += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    return out + b"".join(blobs)


def main():
    images = [draw(size) for size in SIZES]
    target = Path("assets/sqlh.ico")
    target.parent.mkdir(exist_ok=True)
    target.write_bytes(ico(images))
    print(f"{target}: {target.stat().st_size} bytes, sizes {SIZES}, "
          f"images {[len(png(img)) for img in images]}")
    if len(sys.argv) > 1:
        # A preview sheet of every size, at 1x and enlarged, for looking at the result.
        sheet = Image.new("RGBA", (1100, 560), (255, 255, 255, 255))
        x = 10
        for img in images:
            sheet.paste(img, (x, 10), img)
            if img.width <= 64:
                big = img.resize((img.width * 4, img.width * 4), Image.NEAREST)
                sheet.paste(big, (x, 280), big)
            x += max(img.width, img.width * 4 if img.width <= 64 else 0) + 10
        sheet.save(sys.argv[1])


main()
