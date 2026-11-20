#!/usr/bin/env python3
"""Lisa の HUD 用 RGBA 画像を生成する。"""

from pathlib import Path

from PIL import Image, ImageDraw

OUTPUT = Path(__file__).resolve().parents[1] / "assets" / "ui"
PLATE = (10, 20, 31, 120)
RAIL = (255, 255, 255, 230)
INNER = (223, 246, 255, 100)


def background():
    size = 256
    grid = Image.new("RGBA", (size, size))
    draw = ImageDraw.Draw(grid)
    draw.line((0, 0, size - 1, 0), fill=(24, 72, 98, 130))
    draw.line((0, 0, 0, size - 1), fill=(24, 72, 98, 130))
    center = size // 2
    cross = (85, 175, 205, 136)
    draw.line((center - 5, center, center + 5, center), fill=cross)
    draw.line((center, center - 5, center, center + 5), fill=cross)
    return grid


def frame():
    image = Image.new("RGBA", (128, 128))
    draw = ImageDraw.Draw(image)
    points = [(0, 5), (5, 0), (122, 0), (127, 5),
              (127, 122), (122, 127), (5, 127), (0, 122)]
    draw.line(points + points[:1], fill=RAIL, width=2)
    inner = [(3, 7), (7, 3), (120, 3), (124, 7),
             (124, 120), (120, 124), (7, 124), (3, 120)]
    draw.line(inner + inner[:1], fill=INNER, width=1)
    return image


def ear():
    image = Image.new("RGBA", (128, 64))
    draw = ImageDraw.Draw(image)
    draw.polygon([(0, 12), (6, 0), (64, 0), (127, 63), (0, 63)], fill=PLATE)
    draw.line((0, 63, 0, 12), fill=RAIL, width=2)
    draw.line((0, 12, 6, 1), fill=RAIL, width=4)
    draw.rectangle((6, 0, 64, 3), fill=RAIL)
    draw.line((64, 1, 127, 63), fill=RAIL, width=4)
    draw.line((4, 63, 4, 14, 8, 6, 62, 6, 118, 63), fill=INNER, width=2)
    return image


if __name__ == "__main__":
    OUTPUT.mkdir(parents=True, exist_ok=True)
    for name, make in [
        ("bg_hud", background),
        ("folder_frame_9slice", frame),
        ("folder_ear_3slice", ear),
    ]:
        make().save(OUTPUT / f"{name}.png")
