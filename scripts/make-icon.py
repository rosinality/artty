#!/usr/bin/env python3
"""Draw artty's app icon: assets/icon.png (1024px) and assets/artty.icns.

Needs Pillow; the .icns step uses macOS's sips and iconutil.
"""
import os
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = os.path.join(os.path.dirname(__file__), "..")
SIZE = 1024
# macOS icon grid: an 824px rounded square centered in 1024.
INSET, RADIUS = 100, 185


def gradient(size, top, bottom):
    image = Image.new("RGB", (1, size))
    for y in range(size):
        t = y / (size - 1)
        image.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)))
    return image.resize((size, size))


def draw():
    icon = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    box = (INSET, INSET, SIZE - INSET, SIZE - INSET)

    # Soft shadow under the tile.
    shadow = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(shadow).rounded_rectangle((box[0], box[1] + 16, box[2], box[3] + 16), RADIUS, fill=110)
    icon.paste((0, 0, 0, 255), mask=shadow.filter(ImageFilter.GaussianBlur(22)))

    # The tile: deep blue-gray gradient with a faint top highlight.
    mask = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box, RADIUS, fill=255)
    icon.paste(gradient(SIZE, (40, 50, 70), (16, 20, 30)), mask=mask)
    highlight = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(highlight).rounded_rectangle((box[0] + 6, box[1] + 6, box[2] - 6, box[3] - 6), RADIUS - 6, outline=60, width=3)
    icon.paste((255, 255, 255, 255), mask=highlight)

    draw = ImageDraw.Draw(icon)
    # The prompt: ">_".
    font = ImageFont.truetype("/System/Library/Fonts/Menlo.ttc", 330, index=1)
    draw.text((205, 300), ">", font=font, fill=(216, 222, 233, 255))
    draw.text((420, 300), "_", font=font, fill=(143, 180, 255, 255))

    # An amber four-point spark: the agent.
    cx, cy, r = 700, 330, 92
    star = [(cx, cy - r), (cx + r * 0.22, cy - r * 0.22), (cx + r, cy), (cx + r * 0.22, cy + r * 0.22),
            (cx, cy + r), (cx - r * 0.22, cy + r * 0.22), (cx - r, cy), (cx - r * 0.22, cy - r * 0.22)]
    glow = Image.new("L", (SIZE, SIZE), 0)
    ImageDraw.Draw(glow).polygon(star, fill=150)
    icon.paste((224, 175, 104, 255), mask=glow.filter(ImageFilter.GaussianBlur(18)))
    draw.polygon(star, fill=(255, 199, 119, 255))
    return icon


def main():
    icon = draw()
    png = os.path.join(ROOT, "assets", "icon.png")
    icon.save(png)
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "artty.iconset")
        os.mkdir(iconset)
        for points in (16, 32, 128, 256, 512):
            for scale in (1, 2):
                pixels = points * scale
                name = f"icon_{points}x{points}{'@2x' if scale == 2 else ''}.png"
                icon.resize((pixels, pixels), Image.LANCZOS).save(os.path.join(iconset, name))
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", os.path.join(ROOT, "assets", "artty.icns")], check=True)
    print("wrote assets/icon.png and assets/artty.icns")


if __name__ == "__main__":
    main()
