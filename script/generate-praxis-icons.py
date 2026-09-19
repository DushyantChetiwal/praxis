from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
RESOURCES = ROOT / "crates" / "zed" / "resources"


def make_icon(size: int) -> Image.Image:
    scale = size / 1024
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    shadow = Image.new("RGBA", image.size, (0, 0, 0, 0))
    shadow_draw = ImageDraw.Draw(shadow)
    shadow_draw.rounded_rectangle(
        tuple(int(value * scale) for value in (70, 90, 954, 974)),
        radius=int(210 * scale),
        fill=(10, 8, 28, 105),
    )
    image.alpha_composite(shadow)

    tile = Image.new("RGBA", image.size, (0, 0, 0, 0))
    tile_draw = ImageDraw.Draw(tile)
    top = (104, 86, 238)
    bottom = (35, 27, 78)
    for y in range(int(54 * scale), int(946 * scale)):
        progress = (y / scale - 54) / 892
        color = tuple(
            round(start + (end - start) * progress) for start, end in zip(top, bottom)
        )
        tile_draw.line(
            [(int(54 * scale), y), (int(970 * scale), y)],
            fill=(*color, 255),
            width=1,
        )

    mask = Image.new("L", image.size, 0)
    mask_draw = ImageDraw.Draw(mask)
    bounds = tuple(int(value * scale) for value in (54, 54, 970, 970))
    mask_draw.rounded_rectangle(bounds, radius=int(210 * scale), fill=255)
    image.alpha_composite(Image.composite(tile, Image.new("RGBA", image.size), mask))

    draw = ImageDraw.Draw(image)
    draw.rounded_rectangle(
        bounds,
        radius=int(210 * scale),
        outline=(192, 184, 255, 150),
        width=max(1, int(12 * scale)),
    )

    grid_color = (211, 205, 255, 25)
    for coordinate in range(170, 900, 110):
        value = int(coordinate * scale)
        draw.line(
            [(int(120 * scale), value), (int(904 * scale), value)],
            fill=grid_color,
            width=max(1, int(2 * scale)),
        )
        draw.line(
            [(value, int(120 * scale)), (value, int(904 * scale))],
            fill=grid_color,
            width=max(1, int(2 * scale)),
        )

    white = (250, 249, 255, 255)
    stroke = max(2, int(72 * scale))
    stem_x = int(320 * scale)
    draw.line(
        [(stem_x, int(760 * scale)), (stem_x, int(270 * scale))],
        fill=white,
        width=stroke,
    )
    draw.rounded_rectangle(
        tuple(int(value * scale) for value in (284, 234, 754, 594)),
        radius=int(168 * scale),
        outline=white,
        width=stroke,
    )

    accent = (255, 201, 102, 255)
    node_radius = int(28 * scale)
    nodes = ((320, 760), (320, 414), (718, 414))
    draw.line(
        [tuple(int(value * scale) for value in node) for node in nodes],
        fill=(255, 220, 151, 190),
        width=max(2, int(16 * scale)),
        joint="curve",
    )
    for x, y in nodes:
        center_x = int(x * scale)
        center_y = int(y * scale)
        draw.ellipse(
            (
                center_x - node_radius,
                center_y - node_radius,
                center_x + node_radius,
                center_y + node_radius,
            ),
            fill=accent,
            outline=white,
            width=max(1, int(7 * scale)),
        )

    return image


icon_1024 = make_icon(1024)
icon_512 = icon_1024.resize((512, 512), Image.Resampling.LANCZOS)
icon_1024.save(RESOURCES / "app-icon-dev@2x.png")
icon_512.save(RESOURCES / "app-icon-dev.png")
icon_1024.save(
    RESOURCES / "windows" / "app-icon-dev.ico",
    sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
)
