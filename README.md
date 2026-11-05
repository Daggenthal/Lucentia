# Lucentia

A capture app for the Elgato 4K X and other USB capture cards, built for Linux first. Think Elgato Studio, but with full control over how you record. 🎮

## Status

Early days. Right now Lucentia can:

- Open an app window with a live preview, settings, and Record/Stop, with light, dark or system theme

- Find your capture card and list every mode it supports, marked as lossless (uncompressed) or MJPEG (compressed on the card), with 10-bit HDR modes flagged
- Show a live preview with the card's audio, found automatically, and report the real frame rate
- Tone map HDR in the preview so it doesn't look washed out
- Record with H.264, HEVC, AV1 or raw video, at any bitrate, with CBR, VBR or constant quality
- Record 10-bit HDR with correct HDR10 tags
- Record your mic: a Game + Mic track that players play by default, plus separate Game and Mic tracks for editing, with a sync offset
- Estimate file size before you start, and show size, bitrate, lost frames and time left on the drive while recording
- Level meters for game audio and the mic, and mute buttons for both
- Echo cancellation for the mic: removes the game sound your mic picks up from speakers
- Send just the gameplay to OBS, Discord or a browser as a camera (PipeWire camera or v4l2loopback)
- A record hotkey (Alt+F9 by default, through your desktop's global shortcuts) and a `lucentia toggle-record` command
- Check that your GPU can encode each codec, and say why not (GPU and driver) when it can't
- Remember your settings between launches

Tested on: SteamOS with an Elgato 4K X.

## Planned

- Card controls: HDR tone mapping, EDID and VRR settings, color range
- Windows and macOS builds

## AppImage

The easiest way to run Lucentia: download `Lucentia-x86_64.AppImage`, make it executable and run it.

```
chmod +x Lucentia-x86_64.AppImage
./Lucentia-x86_64.AppImage
```

It includes GStreamer and the plugins Lucentia needs, and uses your system's GPU drivers for hardware encoding (VA-API: `mesa` on AMD, `intel-media-driver` on Intel). It runs on distros from 2024 on (glibc 2.39+): Arch, SteamOS, Fedora 40+, Ubuntu 24.04+, Debian 13. Run `./Lucentia-x86_64.AppImage check` to see what it found.

To build it yourself (needs Docker or Podman):

```
packaging/appimage/build-appimage.sh    # writes dist/Lucentia-x86_64.AppImage
```

## Building

Needs Rust and the GStreamer development packages. On Arch (or an Arch distrobox on SteamOS):

```
sudo pacman -S rustup base-devel pkgconf gstreamer gst-plugins-base gst-plugins-good \
  gst-plugins-bad gst-plugin-va gst-plugin-pipewire gst-libav fontconfig libxkbcommon wayland
rustup default stable
cargo run --release
```

## Usage

Run `cargo run --release` to open the app. Open Settings (top right) to pick your card, resolution and frame rate (10-bit HDR shows up when the card offers it) and set the codec and bitrate, then press Record. The Audio and Mic buttons under the video mute your speakers or your mic, even mid-recording. The size estimate updates as you change settings. For 40 fps games (like 40 Hz modes on handhelds), pick "40 fps (from 120 Hz)": Lucentia captures at 120 Hz and keeps every third frame, so set the source to output 120 Hz. On the command line that's `--mode 1920x1080@120 --record-fps 40`. When a recording finishes, Lucentia asks where to save it and what to call it (you can turn that off under Output).

**Streaming the gameplay.** Under Settings, Share, pick a camera. "PipeWire camera" needs no setup and shows up in OBS as "Lucentia (capture card)". For Discord and other apps that only see regular cameras, load v4l2loopback (`sudo modprobe v4l2loopback video_nr=10 card_label=Lucentia exclusive_caps=1`), restart Lucentia and pick the new device. For sound, add the card's audio in OBS or Discord directly.

**Hotkey.** Lucentia asks your desktop for Alt+F9 to start and stop recording (KDE, GNOME and others let you confirm or change it). Where that isn't available, bind `lucentia toggle-record` to a key in your desktop's keyboard settings or in Steam Input.

**Echo cancellation.** If your mic picks up the game from your speakers, turn on Echo cancellation under the mic. It uses the game audio as the reference and subtracts it from the mic. Not needed with headphones.

Everything also works from the terminal:

```
cargo run --release -- list                            # list capture devices and their modes
cargo run --release -- preview                         # live preview at 1080p60
cargo run --release -- preview --mode 1920x1080@120    # any mode from the list
cargo run --release -- preview --mode 2560x1440@60 --hdr
cargo run --release -- preview --mode 3840x2160@144 --mjpeg
cargo run --release -- --help                          # all options
```

### Recording

```
cargo run --release -- record                                   # 1080p60, HEVC, 40 Mbit/s, game audio
cargo run --release -- record --mode 2560x1440@60 --hdr         # 10-bit HDR (AV1 by default)
cargo run --release -- record --codec av1 --quality 20          # constant quality instead of a bitrate
cargo run --release -- record --mic Wave                        # adds Game + Mic, then separate Game and Mic tracks
cargo run --release -- record --analysis --cbr 150              # every frame a keyframe, for frame analysis
cargo run --release -- record --preview --duration 600          # watch while recording, stop after 10 minutes
cargo run --release -- record --mic Wave --echo-cancel          # remove game sound from the mic
cargo run --release -- mics                                     # list microphones for --mic
cargo run --release -- check                                    # which plugins and GPU encoders were found
```

Press Ctrl+C to stop. Recordings go to `~/Videos/Lucentia` unless you pass `--output`.

With `--mic`, the Game + Mic track is mixed when you stop, from the separate Game and Mic tracks. Mixing two live devices in real time came out choppy (each device runs on its own clock); mixing the finished tracks lines them up perfectly. This step is called stitching: the video is copied, not re-encoded, and the app shows how far along it is and about how long is left. If a recording ever stops without finishing (a crash or power cut), the MKV still has the separate tracks, and `lucentia finish FILE.mkv --mix` adds the mix afterwards (`--mp4` converts it too). Use `--mic-volume` and `--game-volume` (in percent) to balance them.

**MKV or MP4?** MKV is the default and the recommendation: if the app, the system or the power cuts out mid-recording, everything up to that moment is still playable, and players and editors show the track names (Game, Mic). Pass `--format mp4` if your editor or a site only takes MP4. It still records as MKV for the same safety, then converts to MP4 when you stop, without re-encoding.

Rough sizes at a fixed bitrate: 20 Mbit/s is about 9 GB per hour, 40 Mbit/s about 18 GB, 80 Mbit/s about 36 GB. Lucentia prints the estimate for your settings before it starts.

Known issue: 10-bit HEVC from the GPU encoder is labeled "Main" instead of "Main 10". It plays fine in most players; AV1 (the default for HDR) doesn't have this problem.

Add `--debug` to any command to also print everything GStreamer reports.

Debug mode can also be turned on with `LUCENTIA_DEBUG=1`. Set `NO_COLOR=1` to turn off colored output.

## Other capture cards

Recent Elgato USB cards (HD60 X, 4K S, Neo, Cam Link) use standard UVC like the 4K X and should work. Older models that need Elgato's own drivers (HD60 S, 4K60 Pro) are not supported on Linux. If you try Lucentia with another card, please open an issue with the output of `--debug`.

## License

MIT. See [LICENSE](LICENSE).

The app window is built with [Slint](https://slint.dev), used under its royalty-free license, which asks for the "About Slint" credit shown in the app's About section. Made with Slint. 💙

Echo cancellation uses [sonora](https://crates.io/crates/sonora), a Rust port of WebRTC's AEC3 (BSD-3-Clause).
