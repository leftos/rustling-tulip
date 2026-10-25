# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "pillow==13.0.0",
#     "resvg-py==0.5.0",
# ]
# ///

"""Builds tools/installer/icon.ico from the icon's SVG sources.

icon.svg is rendered at 256, 64, 48, 32 and 24 px; icon-16.svg is a glyph drawn for 16 px and is used for that size
alone, since the large art shrunk to 16 px loses its prompt. The .ico serves both the installer and the native exe,
whose build script embeds it as icon resource 1.

Run from the repo root: uv run --script tools/installer/icon/build_icon.py
"""

import io
from pathlib import Path

import resvg_py
from PIL import Image

HERE = Path(__file__).resolve().parent
LARGE_SIZES = (256, 64, 48, 32, 24)
SMALL_SIZE = 16


def render(svg: Path, size: int) -> Image.Image:
    """Rasterizes an SVG at size by size pixels on a transparent ground.

    Args:
        svg: The SVG file to render.
        size: The width and height in pixels.

    Returns:
        The RGBA image.
    """
    png = resvg_py.svg_to_bytes(svg_path=str(svg), width=size, height=size)
    return Image.open(io.BytesIO(bytes(png))).convert("RGBA")


def main() -> None:
    frames = [render(HERE / "icon.svg", size) for size in LARGE_SIZES]
    frames.append(render(HERE / "icon-16.svg", SMALL_SIZE))
    out = HERE.parent / "icon.ico"
    sizes = [(frame.width, frame.height) for frame in frames]
    frames[0].save(out, format="ICO", sizes=sizes, append_images=frames[1:])
    print(f"wrote {out} with sizes {[s for s, _ in sizes]}")


if __name__ == "__main__":
    main()
