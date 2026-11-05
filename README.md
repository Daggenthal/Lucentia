# Lucentia

A capture app for the Elgato 4K X and other USB capture cards, built for Linux first. Think Elgato Studio, but with full control over how you record. 🎮

## Status

Early days. Right now Lucentia can:

- Find your capture card and list every mode it supports, marked as lossless (uncompressed) or MJPEG (compressed on the card), with 10-bit HDR modes flagged
- Show a live preview with the card's audio, found automatically, and report the real frame rate

Tested on: SteamOS with an Elgato 4K X.

## Planned

- Pick any bitrate, codec (H.264, HEVC, AV1) and rate control mode
- 10-bit HDR recording with correct HDR10 tags
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
cargo run --release                                    # list capture devices and their modes
cargo run --release -- preview                         # live preview at 1080p60
cargo run --release -- preview --mode 1920x1080@120    # any mode from the list
cargo run --release -- preview --mode 2560x1440@60 --hdr
cargo run --release -- preview --mode 3840x2160@144 --mjpeg
cargo run --release -- --help                          # all options
```

Add `--debug` to any command to also print everything GStreamer reports.

Debug mode can also be turned on with `LUCENTIA_DEBUG=1`. Set `NO_COLOR=1` to turn off colored output.

## Other capture cards

Recent Elgato USB cards (HD60 X, 4K S, Neo, Cam Link) use standard UVC like the 4K X and should work. Older models that need Elgato's own drivers (HD60 S, 4K60 Pro) are not supported on Linux. If you try Lucentia with another card, please open an issue with the output of `--debug`.

## License

MIT. See [LICENSE](LICENSE).
