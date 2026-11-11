# Changelog

## 0.1.1

- Smoother forward frame stepping with a persistent decoder and bounded rolling buffer.
- Fixed navigation freezes when reversing direction after cached frames were evicted.
- Held arrow, 10-frame, and keyframe navigation waits for the requested preview.
- Shared timing indexes reduce copying during navigation; upcoming navigation targets are prefetched.
- Added preview latency diagnostics in Tools → FFmpeg Capabilities & Cache Telemetry.
- Export presets are grouped by codec, including high-quality HEVC and additional H.264/AV1 options.
- Replaced the bundled VP9 preset with AV1 + Opus WebM; unavailable encoder presets are hidden.
- Added startup guidance when FFmpeg/ffprobe is missing or cannot run, and verified external VVC discovery.
- Added the CutDub Split Play icon to app windows, macOS bundles, Windows executables, and Linux launchers.
- Added collapsible filters and improved audio encoding controls and bitrate defaults.
- The preset selector shows the selected preset, switches to Custom after changes, and reports save failures.
- Added Linux release packaging, automatic checksums, and ad-hoc macOS bundle signing.

FFmpeg and ffprobe are required separately. Optional VVC encoding requires native
FFmpeg libvvenc support or a working vvencapp installation. macOS packages are
ad-hoc signed and are not notarized. Linux binaries are built on Ubuntu 24.04.
