# Lucentia

A capture app for the Elgato 4K X and other USB capture cards, built for Linux first. Think Elgato Studio, but with full control over how you record. 🎮

## Status

Early days. Right now Lucentia finds your capture card and lists every mode it supports, marked as lossless (uncompressed) or MJPEG (compressed on the card).

Tested on: SteamOS with an Elgato 4K X.

## Planned

- Pick any bitrate, codec (H.264, HEVC, AV1) and rate control mode
- 10-bit HDR recording with correct HDR10 tags
- Live preview with sound and no added latency
- Mic support: Separate game and mic tracks, level meters, live monitoring and a sync offset
- AppImage for any Linux distro, with Windows and macOS builds later

## Building

Needs Rust and the GStreamer development packages. On Arch (or an Arch distrobox on SteamOS):

```
sudo pacman -S rustup base-devel pkgconf gstreamer gst-plugins-base gst-plugins-good
rustup default stable
cargo run --release
```

## Usage

```
cargo run --release            # list capture devices and their modes
cargo run --release -- --debug # also print everything GStreamer reports
```

Debug mode can also be turned on with `LUCENTIA_DEBUG=1`. Set `NO_COLOR=1` to turn off colored output.

## Other capture cards

Recent Elgato USB cards (HD60 X, 4K S, Neo, Cam Link) use standard UVC like the 4K X and should work. Older models that need Elgato's own drivers (HD60 S, 4K60 Pro) are not supported on Linux. If you try Lucentia with another card, please open an issue with the output of `--debug`.

## License

MIT. See [LICENSE](LICENSE).
