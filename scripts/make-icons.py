#!/usr/bin/env python3
"""Build the app icons on the macOS icon grid.

A macOS app icon is a 1024-point canvas whose artwork sits in an 824-point
rounded square, leaving 100 points of transparent margin; the Dock, Finder and
Launchpad expect that geometry and do not add it themselves. The source PNGs are
full-bleed squares, so they read as plain squares in the Dock until they are
placed on this grid.

Run from the repository root (needs Pillow):

    python3 scripts/make-icons.py

Reads  ~/Desktop/mizar-icon/{light-icon,dark-icon}.png
Writes crates/agi-app/icons/{icon-light,icon-dark,icon}.png, the Tauri PNG
sizes, icon.icns, icon-dark.icns and icon.ico.
"""

from pathlib import Path

from PIL import Image, ImageDraw

CANVAS = 1024
TILE = 824
MARGIN = (CANVAS - TILE) // 2
# Apple's grid: the 824-point tile's corners have a radius of about 185 points.
RADIUS = 185
SOURCE = Path.home() / "Desktop" / "mizar-icon"
ICONS = Path(__file__).resolve().parent.parent / "crates" / "agi-app" / "icons"


def mask() -> Image.Image:
    """The rounded-square mask, drawn large and shrunk so the edge is smooth."""
    scale = 4
    size = TILE * scale
    image = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((0, 0, size - 1, size - 1), radius=RADIUS * scale, fill=255)
    return image.resize((TILE, TILE), Image.LANCZOS)


def master(name: str, background: tuple[int, int, int]) -> Image.Image:
    source = Image.open(SOURCE / name).convert("RGBA")
    # The sources carry a one-pixel soft edge; flatten onto the tile's own
    # background so the mask, not the source, decides the outline.
    flat = Image.new("RGBA", source.size, (*background, 255))
    flat.alpha_composite(source)
    tile = flat.resize((TILE, TILE), Image.LANCZOS)
    tile.putalpha(mask())
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.alpha_composite(tile, (MARGIN, MARGIN))
    return canvas


def main() -> None:
    ICONS.mkdir(parents=True, exist_ok=True)
    light = master("light-icon.png", (255, 255, 255))
    dark = master("dark-icon.png", (0, 0, 0))
    light.save(ICONS / "icon-light.png")
    dark.save(ICONS / "icon-dark.png")
    light.save(ICONS / "icon.png")

    for size, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")):
        light.resize((size, size), Image.LANCZOS).save(ICONS / name)

    light.save(
        ICONS / "icon.ico",
        sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    print("icons written to", ICONS)


if __name__ == "__main__":
    main()
