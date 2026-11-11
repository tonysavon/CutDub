# CutDub icon: Split Play

An ivory and mint play symbol separated by a precise vertical cut, on a midnight
rounded tile. No lettering or fine details, so the mark remains legible at small sizes.

- `cutdub.svg`: editable master, 1024 × 1024, transparent outside the tile.
- `cutdub.png`: full-size preview.
- `cutdub-256.png`: embedded window/taskbar icon and Linux raster icon.
- `cutdub.ico`: Windows executable resource, 16–256 px in seven sizes.
- `CutDub.icns`: macOS bundle icon, standard and Retina sizes up to 1024 px.

Regenerate with `python3 scripts/generate-icons.py` from any directory. Requires
librsvg's `rsvg-convert`; run on macOS to regenerate ICNS using `iconutil` too.
The generated assets are committed, so building the app needs no image tools.

Windows builds embed the ICO automatically through `crates/redub-app/build.rs`.
macOS Finder/Dock icons come from the `.app` produced by `scripts/package-macos.sh`;
a bare Mach-O command-line executable does not have a bundle icon. Linux packages
include hicolor icons alongside the desktop entry; install the package's `bin` and
`share` directories under the same application prefix for launcher discovery.
