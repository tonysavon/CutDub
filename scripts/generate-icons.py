#!/usr/bin/env python3
"""Regenerate committed app icons from the SVG; requires librsvg's rsvg-convert.

Run on macOS to also regenerate the ICNS using Apple's iconutil.
Normal application builds use the committed assets and need neither tool.
"""
import pathlib
import shutil
import struct
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
ICONS = ROOT / "assets" / "icons"
SVG = ICONS / "cutdub.svg"


def main():
    if not shutil.which("rsvg-convert"):
        sys.exit("Install librsvg (rsvg-convert) to regenerate icons.")
    with tempfile.TemporaryDirectory(prefix="cutdub-icons-") as temp:
        work = pathlib.Path(temp)
        images = {}
        for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
            png = work / f"{size}.png"
            subprocess.run(["rsvg-convert", "-w", str(size), "-h", str(size),
                            "-o", str(png), str(SVG)], check=True)
            images[size] = png.read_bytes()
        (ICONS / "cutdub.png").write_bytes(images[1024])
        (ICONS / "cutdub-256.png").write_bytes(images[256])
        sizes = (16, 24, 32, 48, 64, 128, 256)
        offset = 6 + 16 * len(sizes)
        entries = []
        for size in sizes:
            data = images[size]
            entries.append(struct.pack("<BBBBHHII", size % 256, size % 256,
                                       0, 0, 1, 32, len(data), offset))
            offset += len(data)
        (ICONS / "cutdub.ico").write_bytes(
            struct.pack("<HHH", 0, 1, len(sizes)) + b"".join(entries)
            + b"".join(images[size] for size in sizes))
        if sys.platform == "darwin":
            iconset = work / "CutDub.iconset"
            iconset.mkdir()
            for size in (16, 32, 128, 256, 512):
                (iconset / f"icon_{size}x{size}.png").write_bytes(images[size])
                (iconset / f"icon_{size}x{size}@2x.png").write_bytes(images[size * 2])
            subprocess.run(["iconutil", "-c", "icns", str(iconset),
                            "-o", str(ICONS / "CutDub.icns")], check=True)


if __name__ == "__main__":
    main()
