#!/usr/bin/env python3
"""Render assets/oled-preview.png exactly as the firmware draws the panel.

The point of this script is that the README image cannot quietly drift away
from the code: the glyphs come out of embedded-graphics' own font sheets, the
bird comes out of the generated src/logo.rs, and the coordinates below are the
same ones main.rs passes to `Text::with_baseline` and `Image::new`.

    python3 tools/gen-oled-preview.py      # needs Pillow and a fetched cargo registry

Rerun it after changing the display task's layout.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

from PIL import Image

OUT = Path("assets/oled-preview.png")
LOGO_RS = Path("src/logo.rs")

# Panel geometry.
WIDTH, HEIGHT = 128, 64

# Presentation only -- these values are not in the firmware.
SCALE = 4
FRAME = 2
MARGIN = 20
GAP = 28
COLOUR_OFF = (6, 9, 13, 255)
COLOUR_ON = (222, 240, 255, 255)
COLOUR_FRAME = (48, 54, 61, 255)

# The sample values the status screen is drawn with.
SAMPLE_READING = "41.7 °C"
SAMPLE_STATUS = "chip  up 42 s"


# ---------------------------------------------------------------------------
# embedded-graphics font sheets
# ---------------------------------------------------------------------------


def embedded_graphics_root() -> Path:
    """Locate the embedded-graphics source in the cargo registry."""
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1"],
            capture_output=True,
            check=True,
            text=True,
        ).stdout
    )
    for package in meta["packages"]:
        if package["name"] == "embedded-graphics":
            return Path(package["manifest_path"]).parent
    sys.exit("embedded-graphics not in the dependency graph; run `cargo fetch` first")


class MonoFont:
    """One embedded-graphics `MonoFont`, enough of it to blit text."""

    def __init__(self, sheet: Path, sheet_width: int, size: tuple[int, int], ranges):
        self.width, self.height = size
        self.sheet_width = sheet_width
        self.columns = sheet_width // self.width
        self.data = sheet.read_bytes()
        self.stride = sheet_width // 8
        # `StrGlyphMapping` assigns consecutive indices to consecutive ranges.
        self.index = {}
        next_index = 0
        for start, end in ranges:
            for code in range(start, end + 1):
                self.index[chr(code)] = next_index
                next_index += 1
        # `StrGlyphMapping::new(.., '?' as usize - ' ' as usize)`.
        self.replacement = ord("?") - ord(" ")

    def pixel(self, glyph: int, x: int, y: int) -> bool:
        gx = (glyph % self.columns) * self.width + x
        gy = (glyph // self.columns) * self.height + y
        byte = self.data[gy * self.stride + gx // 8]
        return bool(byte & (0x80 >> (gx % 8)))

    def draw(self, canvas, text: str, origin: tuple[int, int]) -> None:
        """Draw `text` with its glyph boxes' top edge at `origin`.

        That is `Text::with_baseline(.., Baseline::Top)`; character_spacing is
        zero for both fonts used here.
        """
        ox, oy = origin
        for i, char in enumerate(text):
            glyph = self.index.get(char, self.replacement)
            for y in range(self.height):
                for x in range(self.width):
                    if self.pixel(glyph, x, y):
                        canvas[ox + i * self.width + x, oy + y] = True


# ---------------------------------------------------------------------------
# The generated logo
# ---------------------------------------------------------------------------


def load_logo(name: str) -> tuple[list[list[bool]], int, int]:
    """Decode one `ImageRaw` back out of src/logo.rs."""
    source = LOGO_RS.read_text()

    width = int(
        re.search(rf"ImageRaw::new\(&{name}_DATA, (\d+)\)", source).group(1)  # type: ignore[union-attr]
    )
    body = re.search(
        rf"const {name}_DATA: \[u8; \d+\] = \[(.*?)\];", source, re.S
    ).group(1)  # type: ignore[union-attr]
    data = [int(b, 0) for b in re.findall(r"0x[0-9a-fA-F]{2}", body)]

    stride = width // 8
    height = len(data) // stride
    rows = [
        [bool(data[y * stride + x // 8] & (0x80 >> (x % 8))) for x in range(width)]
        for y in range(height)
    ]
    return rows, width, height


def blit(canvas, logo, origin: tuple[int, int]) -> None:
    rows, width, height = logo
    ox, oy = origin
    for y in range(height):
        for x in range(width):
            if rows[y][x]:
                canvas[ox + x, oy + y] = True


# ---------------------------------------------------------------------------
# The two screens, drawn with main.rs's coordinates
# ---------------------------------------------------------------------------


class Canvas:
    def __init__(self):
        self.pixels = [[False] * WIDTH for _ in range(HEIGHT)]

    def __setitem__(self, xy, value):
        x, y = xy
        if 0 <= x < WIDTH and 0 <= y < HEIGHT:
            self.pixels[y][x] = value

    def border(self):
        """`Rectangle::new(Point::zero(), Size::new(128, 64))` with a 1 px
        centre-aligned stroke, which lands entirely inside the panel."""
        for x in range(WIDTH):
            self[x, 0] = True
            self[x, HEIGHT - 1] = True
        for y in range(HEIGHT):
            self[0, y] = True
            self[WIDTH - 1, y] = True


def splash(small, large, logo_large) -> Canvas:
    canvas = Canvas()
    blit(canvas, logo_large, (4, 8))
    small.draw(canvas, "kolibri", (76, 20))
    small.draw(canvas, "esp32-c3", (76, 34))
    return canvas


def status(small, big, logo_small) -> Canvas:
    canvas = Canvas()
    canvas.border()
    blit(canvas, logo_small, (86, 3))
    small.draw(canvas, "kolibri", (5, 4))
    big.draw(canvas, SAMPLE_READING, (5, 20))
    small.draw(canvas, SAMPLE_STATUS, (5, 46))
    return canvas


def render(canvas: Canvas) -> Image.Image:
    panel = Image.new("RGBA", (WIDTH, HEIGHT), COLOUR_OFF)
    pixels = panel.load()
    for y in range(HEIGHT):
        for x in range(WIDTH):
            if canvas.pixels[y][x]:
                pixels[x, y] = COLOUR_ON
    panel = panel.resize((WIDTH * SCALE, HEIGHT * SCALE), Image.NEAREST)

    framed = Image.new(
        "RGBA", (panel.width + 2 * FRAME, panel.height + 2 * FRAME), COLOUR_FRAME
    )
    framed.paste(panel, (FRAME, FRAME))
    return framed


def main() -> None:
    root = embedded_graphics_root()
    small = MonoFont(
        root / "fonts/raw/ascii/font_6x10.raw", 96, (6, 10), [(0x20, 0x7F)]
    )
    big = MonoFont(
        root / "fonts/raw/iso_8859_1/font_10x20.raw",
        160,
        (10, 20),
        [(0x20, 0x7F), (0xA0, 0xFF)],
    )

    screens = [
        render(splash(small, big, load_logo("LARGE"))),
        render(status(small, big, load_logo("SMALL"))),
    ]

    width = 2 * MARGIN + sum(s.width for s in screens) + GAP
    height = 2 * MARGIN + screens[0].height
    sheet = Image.new("RGBA", (width, height), (0, 0, 0, 0))
    x = MARGIN
    for screen in screens:
        sheet.paste(screen, (x, MARGIN))
        x += screen.width + GAP

    OUT.parent.mkdir(exist_ok=True)
    sheet.save(OUT)
    print(f"wrote {OUT} ({sheet.width}x{sheet.height})")


if __name__ == "__main__":
    main()
