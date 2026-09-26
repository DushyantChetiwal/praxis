"""Generates every Praxis icon from the brand art in docs/brand/praxis-art.png.

Compact assets (application, installer, document, and favicon icons) use only the
"P" mark. The wordmark and subtitle are reserved for large surfaces such as the
README, where docs/brand/praxis-banner.png is used.

Run with `python script/generate-praxis-icons.py` after changing the brand art.
"""

from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
RESOURCES = ROOT / "crates" / "zed" / "resources"
BRAND = ROOT / "docs" / "brand"
SOURCE_ART = BRAND / "praxis-art.png"

TILE_TOP = (38, 36, 33)
TILE_BOTTOM = (22, 21, 19)
TILE_OUTLINE = (236, 230, 218, 38)
MARK_HEIGHT_RATIO = 0.56
CHANNELS = ("", "-dev", "-nightly", "-preview")
ICO_SIZES = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]


def alpha_bounds(image: Image.Image, top: int, bottom: int) -> tuple[int, int, int, int]:
    region = image.crop((0, top, image.width, bottom))
    solid = region.split()[3].point(lambda value: 255 if value > 60 else 0)
    bounds = solid.getbbox()
    if bounds is None:
        raise SystemExit(f"No artwork found between rows {top} and {bottom} of {SOURCE_ART}")
    left, upper, right, lower = bounds
    return left, upper + top, right, lower + top


def extract_mark(art: Image.Image) -> Image.Image:
    # The mark sits above the wordmark; everything below this row is text.
    wordmark_row = int(art.height * 0.57)
    left, top, right, bottom = alpha_bounds(art, 0, wordmark_row)
    padding = 4
    return art.crop((left - padding, top - padding, right + padding, bottom + padding))


def make_tile(size: int, mark: Image.Image) -> Image.Image:
    scale = size / 1024
    inset = int(64 * scale)
    radius = int(224 * scale)
    bounds = (inset, inset, size - inset, size - inset)

    gradient = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(gradient)
    span = max(1, bounds[3] - bounds[1])
    for y in range(bounds[1], bounds[3]):
        progress = (y - bounds[1]) / span
        color = tuple(
            round(start + (end - start) * progress) for start, end in zip(TILE_TOP, TILE_BOTTOM)
        )
        draw.line([(bounds[0], y), (bounds[2], y)], fill=(*color, 255))

    mask = Image.new("L", (size, size), 0)
    ImageDraw.Draw(mask).rounded_rectangle(bounds, radius=radius, fill=255)
    icon = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    icon.paste(gradient, (0, 0), mask)

    ImageDraw.Draw(icon).rounded_rectangle(
        bounds,
        radius=radius,
        outline=TILE_OUTLINE,
        width=max(1, int(6 * scale)),
    )

    target_height = int(size * MARK_HEIGHT_RATIO)
    target_width = round(mark.width * target_height / mark.height)
    scaled_mark = mark.resize((target_width, target_height), Image.Resampling.LANCZOS)
    offset = ((size - target_width) // 2, (size - target_height) // 2)
    icon.alpha_composite(scaled_mark, offset)
    return icon


def make_banner(art: Image.Image) -> Image.Image:
    left, top, right, bottom = alpha_bounds(art, 0, art.height)
    padding = 96
    artwork = art.crop((left - padding, top - padding, right + padding, bottom + padding))
    banner = Image.new("RGBA", artwork.size, (*TILE_BOTTOM, 255))
    banner.alpha_composite(artwork)
    return banner


def main() -> None:
    art = Image.open(SOURCE_ART).convert("RGBA")
    mark = extract_mark(art)
    icon_1024 = make_tile(1024, mark)
    icon_512 = icon_1024.resize((512, 512), Image.Resampling.LANCZOS)

    for channel in CHANNELS:
        icon_1024.save(RESOURCES / f"app-icon{channel}@2x.png")
        icon_512.save(RESOURCES / f"app-icon{channel}.png")
        icon_1024.save(RESOURCES / "windows" / f"app-icon{channel}.ico", sizes=ICO_SIZES)

    icon_1024.save(ROOT / "crates" / "auto_update_helper" / "app-icon.ico", sizes=ICO_SIZES)
    icon_1024.save(RESOURCES / "Document.icns")
    icon_1024.resize((64, 64), Image.Resampling.LANCZOS).save(ROOT / "docs" / "theme" / "favicon.png")
    icon_512.save(ROOT / "crates" / "gpui" / "examples" / "image" / "app-icon.png")

    mark_512 = mark.resize(
        (round(mark.width * 512 / mark.height), 512), Image.Resampling.LANCZOS
    )
    mark_512.save(BRAND / "praxis-mark.png")
    make_banner(art).save(BRAND / "praxis-banner.png")


if __name__ == "__main__":
    main()
