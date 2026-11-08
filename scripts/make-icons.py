#!/usr/bin/env python3
"""Build the app icons on the macOS icon grid.

A macOS app icon is a 1024-point canvas whose artwork sits in an 824-point
rounded square, leaving 100 points of transparent margin; the Dock, Finder and
Launchpad expect that geometry and do not add it themselves. The source PNGs are
full-bleed squares, so they read as plain squares in the Dock until they are
placed on this grid.

The same source also gives the Tauri/Windows PNGs, both `.icns` bundles, the
Windows `.ico`, and the menu-bar cut-out. Nothing is redrawn: the sources are
the owner's masters, and this script only composes them onto the platform
geometry.

    python3 scripts/make-icons.py [--source-dir PATH]

Sources, in order of preference:

    <workspace>/icon-design/masters/mizar-icon/{light,dark,light-transparent,dark-transparent}.png
    ~/Desktop/mizar-icon/{light,light2,dark,dark2}.png

Writes `crates/agi-app/icons/`: icon-light.png, icon-dark.png, icon.png,
32x32.png, 128x128.png, 128x128@2x.png, icon.icns, icon-dark.icns, icon.ico and
tray-white.png. `iconutil` is needed for the `.icns` files; the PNGs and `.ico`
are written even when it is unavailable.
"""

from __future__ import annotations

import argparse
import importlib.util
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

CANVAS = 1024
TILE = 824
MARGIN = (CANVAS - TILE) // 2
# Apple's grid: the 824-point tile's corners have a radius of about 185 points.
RADIUS = 185

ICONS = Path(__file__).resolve().parent.parent / "crates" / "agi-app" / "icons"
WORKSPACE = Path(__file__).resolve().parent.parent.parent
MASTER_DIR = WORKSPACE / "icon-design" / "masters" / "mizar-icon"
LEGACY_DIR = Path.home() / "Desktop" / "mizar-icon"


def find_sources(source_dir: Path | None) -> dict[str, Path]:
    """Locate the four source images, accepting the master and legacy names."""
    if source_dir is not None:
        candidates = [source_dir]
    else:
        candidates = [MASTER_DIR, LEGACY_DIR]
    for directory in candidates:
        names = {
            "light": directory / "light.png",
            "dark": directory / "dark.png",
            "light-mark": directory / "light-transparent.png",
            "dark-mark": directory / "dark-transparent.png",
        }
        if not names["light"].exists() and (directory / "light-original.jpg").exists():
            names["light"] = directory / "light-original.jpg"
        # The names used on the owner's Desktop before the masters were copied
        # into the workspace.
        legacy = {
            "light": directory / "light.png",
            "dark": directory / "dark.png",
            "light-mark": directory / "light2.png",
            "dark-mark": directory / "dark2.png",
        }
        for key, path in list(names.items()):
            if not path.exists() and legacy[key].exists():
                names[key] = legacy[key]
        if all(path.exists() for path in names.values()):
            return names
    searched = "\n  ".join(str(path) for path in candidates)
    raise SystemExit(f"no icon sources found in:\n  {searched}")


def corner_colour(image: Image.Image) -> tuple[int, int, int]:
    """The tile's own background, sampled from its four corners.

    The sources are full-bleed, so the corners are the background rather than
    transparency. Taking the median of four small corner patches survives a
    one-pixel soft edge and a stray JPEG artefact.
    """
    rgb = image.convert("RGB")
    size = 12
    samples: list[tuple[int, int, int]] = []
    for x in (1, rgb.width - size - 1):
        for y in (1, rgb.height - size - 1):
            patch = rgb.crop((x, y, x + size, y + size))
            samples.append(tuple(patch.resize((1, 1), Image.LANCZOS).getpixel((0, 0))))
    channels = list(zip(*samples))
    return tuple(sorted(channel)[len(channel) // 2] for channel in channels)


def mask() -> Image.Image:
    """The rounded-square mask, drawn large and shrunk so the edge is smooth."""
    scale = 4
    size = TILE * scale
    image = Image.new("L", (size, size), 0)
    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle((0, 0, size - 1, size - 1), radius=RADIUS * scale, fill=255)
    return image.resize((TILE, TILE), Image.LANCZOS)


def master(source: Path) -> Image.Image:
    """One appearance on the macOS 1024 canvas."""
    image = Image.open(source).convert("RGBA")
    background = corner_colour(image)
    # The sources carry a one-pixel soft edge; flatten onto the tile's own
    # background so the mask, not the source, decides the outline.
    flat = Image.new("RGBA", image.size, (*background, 255))
    flat.alpha_composite(image)
    tile = flat.resize((TILE, TILE), Image.LANCZOS)
    tile.putalpha(mask())
    canvas = Image.new("RGBA", (CANVAS, CANVAS), (0, 0, 0, 0))
    canvas.alpha_composite(tile, (MARGIN, MARGIN))
    return canvas


def mark(source: Path) -> Image.Image:
    """The mark alone, white on transparency, for the menu bar and templates."""
    geometry = MASTER_DIR / "ring_geometry.py"
    if geometry.exists():
        # The status item is the same five arcs as the app, not a thresholded
        # copy of the raster: at 18 points the stepped caps show.
        spec = importlib.util.spec_from_file_location("mizar_ring_geometry", geometry)
        if spec is not None and spec.loader is not None:
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)
            return module.render(128, padding=0.04, supersample=8)

    image = Image.open(source).convert("RGBA")
    alpha = image.getchannel("A")
    # The transparent sources also carry the glyph's soft shadow. A status item
    # is a cut-out, not a picture, so drop the shadow and keep the ink.
    alpha = alpha.point(lambda value: 0 if value < 96 else 255)
    alpha = alpha.filter(ImageFilter.GaussianBlur(0.6))
    bbox = alpha.getbbox()
    if bbox is None:
        raise SystemExit(f"{source} has no visible mark")
    alpha = alpha.crop(bbox)
    side = max(alpha.size)
    canvas = Image.new("L", (side, side), 0)
    canvas.paste(alpha, ((side - alpha.width) // 2, (side - alpha.height) // 2))
    white = Image.new("RGBA", (side, side), (255, 255, 255, 0))
    white.putalpha(canvas)
    # A little breathing room inside the 18-point status item.
    pad = max(1, round(side * 0.04))
    padded = Image.new("RGBA", (side + 2 * pad, side + 2 * pad), (0, 0, 0, 0))
    padded.alpha_composite(white, (pad, pad))
    return padded.resize((128, 128), Image.LANCZOS)


def write_icns(canvas: Image.Image, destination: Path) -> bool:
    """Write an `.icns` via `iconutil`, or leave the old file if unavailable."""
    iconutil = shutil.which("iconutil")
    if iconutil is None:
        print(f"iconutil not found; skipped {destination.name}", file=sys.stderr)
        return False
    with tempfile.TemporaryDirectory(prefix="mizar-iconset-") as temporary:
        root = Path(temporary)
        iconset = root / "icon.iconset"
        iconset.mkdir()
        for points in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = points * scale
                name = f"icon_{points}x{points}{'@2x' if scale == 2 else ''}.png"
                canvas.resize((pixels, pixels), Image.LANCZOS).save(iconset / name)
        result = subprocess.run(
            [iconutil, "-c", "icns", "-o", str(destination), str(iconset)],
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            print(f"iconutil failed for {destination.name}: {result.stderr.strip()}", file=sys.stderr)
            return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", type=Path, default=None)
    arguments = parser.parse_args()

    sources = find_sources(arguments.source_dir)
    ICONS.mkdir(parents=True, exist_ok=True)

    light = master(sources["light"])
    dark = master(sources["dark"])
    light.save(ICONS / "icon-light.png")
    dark.save(ICONS / "icon-dark.png")
    light.save(ICONS / "icon.png")

    for size, name in ((32, "32x32.png"), (128, "128x128.png"), (256, "128x128@2x.png")):
        light.resize((size, size), Image.LANCZOS).save(ICONS / name)

    light.save(
        ICONS / "icon.ico",
        sizes=[(16, 16), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )
    mark(sources["light-mark"]).save(ICONS / "tray-white.png")

    write_icns(light, ICONS / "icon.icns")
    write_icns(dark, ICONS / "icon-dark.icns")
    print("icons written to", ICONS)
    return 0


if __name__ == "__main__":
    sys.exit(main())
