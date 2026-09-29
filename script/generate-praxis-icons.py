"""Renders every raster Praxis icon from the SVG sources in docs/brand.

The SVGs are the brand sources:

- praxis-app-icon.svg: the application tile, rendered to the PNG, ICO, and ICNS
  icons that platform packaging requires, and copied as the docs favicon.
- praxis-mark.svg: the "P" mark on its own.
- praxis-logo-dark.svg / praxis-logo-light.svg: the full logo with wordmark for
  dark and light backgrounds, used by the README. Their text is outlined, so
  they render the same everywhere without fonts installed.

Each size is rendered straight from the vector rather than downscaled from one
large bitmap, which keeps small icons sharp.

Run with `python script/generate-praxis-icons.py` after changing the SVGs.
Requires `pip install pillow resvg-py`.
"""

import io
import shutil
from pathlib import Path

import resvg_py
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
RESOURCES = ROOT / "crates" / "zed" / "resources"
BRAND = ROOT / "docs" / "brand"
APP_ICON = BRAND / "praxis-app-icon.svg"

CHANNELS = ("", "-dev", "-nightly", "-preview")
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)
ICNS_SIZES = (16, 32, 64, 128, 256, 512, 1024)


def render(size: int) -> Image.Image:
    png = resvg_py.svg_to_bytes(svg_path=str(APP_ICON), width=size, height=size)
    return Image.open(io.BytesIO(bytes(png))).convert("RGBA")


def save_multi_size(path: Path, sizes: tuple[int, ...], image_format: str) -> None:
    images = [render(size) for size in sizes]
    largest = images[-1]
    largest.save(
        path,
        format=image_format,
        sizes=[(size, size) for size in sizes],
        append_images=images[:-1],
    )


def main() -> None:
    icon_1024 = render(1024)
    icon_512 = render(512)

    for channel in CHANNELS:
        icon_1024.save(RESOURCES / f"app-icon{channel}@2x.png")
        icon_512.save(RESOURCES / f"app-icon{channel}.png")
        save_multi_size(RESOURCES / "windows" / f"app-icon{channel}.ico", ICO_SIZES, "ICO")

    save_multi_size(ROOT / "crates" / "auto_update_helper" / "app-icon.ico", ICO_SIZES, "ICO")
    save_multi_size(RESOURCES / "Document.icns", ICNS_SIZES, "ICNS")
    render(64).save(ROOT / "docs" / "theme" / "favicon.png")
    shutil.copyfile(APP_ICON, ROOT / "docs" / "theme" / "favicon.svg")
    icon_512.save(ROOT / "crates" / "gpui" / "examples" / "image" / "app-icon.png")


if __name__ == "__main__":
    main()
