//! Lucentia: Capture app for Elgato (and other UVC) capture cards.
//!
//! Commands:
//!   lucentia                 list capture devices and their modes
//!   lucentia preview [opts]  live preview with sound
//!   lucentia record [opts]   record to a file
//!   lucentia mics            list microphones
//! Add --debug (or LUCENTIA_DEBUG=1) to see everything GStreamer reports.

mod devices;
mod encode;
mod estimate;
mod log;
mod preview;
mod record;
mod remux;

use devices::{Kind, Mode};
use encode::{AudioCodec, Codec, RateControl};
use gst::prelude::*;
use gstreamer as gst;
use std::path::PathBuf;

const USAGE: &str = "\
Usage:
  lucentia                      List capture devices and the modes they support
  lucentia preview [options]    Live preview with sound
  lucentia record [options]     Record to a file (Ctrl+C to stop)
  lucentia mics                 List microphones
  lucentia finish FILE.mkv [--mp4] [--mix]
                                Redo the finishing step on a recording: add the
                                Game + Mic track and/or convert to MP4 (handy after a crash)

Capture options (preview and record):
  --mode WxH@FPS   Mode to open, e.g. 1920x1080@60 or 3840x2160@59.94 (default 1080p60)
  --hdr            Use the 10-bit HDR version of the mode
  --mjpeg          Use the card's MJPEG version of the mode (needed for 4K144)
  --hdr-signal     The source is sending HDR in an 8-bit mode (e.g. 4K60 HDR)
  --no-audio       No game audio
  --device N       Which card to use when more than one is connected (default 1)

Record options:
  --codec C        h264, hevc, av1 or raw (default: hevc, or av1 for HDR)
  --cbr MBPS       Fixed bitrate in Mbit/s (default: 40 at 1080p60, scaled by mode)
  --vbr MBPS[:MAX] Variable bitrate: average, and optional ceiling (default 1.5x)
  --quality Q      Constant quality, 0 (best) to 51 (worst); about 18 to 23 is good
  --keyframe SECS  Seconds between keyframes (default 2)
  --analysis       Every frame a keyframe, for frame-accurate analysis (big files)
  --audio C        aac, opus or pcm (default aac)
  --audio-bitrate K  Audio bitrate in kbit/s (default 320)
  --mic NAME       Also record a mic as its own track (part of its name, or default)
  --mic-offset MS  Shift the mic in milliseconds to line it up with the video
  --mic-volume PCT Mic volume in percent (default 100; try 50 if it's too loud)
  --game-volume PCT  Game volume in percent (default 100)
  --no-mix-track   With --mic, leave out the \"Game + Mic\" track. By default it's the
                   first audio track (what players play), followed by separate Game
                   and Mic tracks for editing
  --format F       mkv (default) or mp4. MKV is recommended: if the app, the system
                   or the power cuts out mid-recording, everything up to that moment
                   is still playable, and players show the track names (Game, Mic).
                   MP4 is recorded as MKV for the same safety, then converted when you
                   stop, for editors or sites that only take MP4.
  --output PATH    Where to save (default ~/Videos/Lucentia/Lucentia <date>.mkv)
  --duration SECS  Stop by itself after this long
  --preview        Show a preview window (with game audio) while recording

Global options:
  --debug, -d      Full logging
  --help, -h       This help";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }
    let debug = args.iter().any(|a| a == "--debug" || a == "-d")
        || std::env::var("LUCENTIA_DEBUG").is_ok_and(|v| v != "0");
    log::init_debug(debug);

    // The OpenGL preview window opens tiny and can't be resized when GStreamer picks
    // its Wayland window. Its X11 window (through XWayland) sizes itself to the video,
    // so use that when an X display exists. Set GST_GL_WINDOW yourself to override.
    // This must happen before GStreamer starts any threads.
    if cfg!(target_os = "linux")
        && std::env::var_os("GST_GL_WINDOW").is_none()
        && std::env::var_os("DISPLAY").is_some()
    {
        std::env::set_var("GST_GL_WINDOW", "x11");
        log::debug("using the X11 OpenGL window for the preview");
    }

    if let Err(e) = gst::init() {
        log::error(&format!("GStreamer failed to start: {e}"));
        std::process::exit(1);
    }
    log::debug(&format!("GStreamer {}", gst::version_string()));

    let cmd = args
        .first()
        .map(String::as_str)
        .filter(|c| !c.starts_with('-'));
    match cmd {
        Some("mics") => return list_mics(),
        Some("finish") => return run_finish(&args[1..]),
        Some("preview") | Some("record") | None => {}
        Some(other) => {
            log::error(&format!("unknown command `{other}`\n\n{USAGE}"));
            std::process::exit(64);
        }
    }

    let test_source = args.iter().any(|a| a == "--test-source");
    let devs = match devices::list() {
        Ok(d) => d,
        Err(e) => {
            log::error(&e);
            std::process::exit(1);
        }
    };
    if devs.is_empty() && !test_source {
        log::warn("no capture devices found. Is the card plugged in?");
        std::process::exit(2);
    }

    match cmd {
        Some("preview") => run_preview(&args[1..], &devs),
        Some("record") => run_record(&args[1..], &devs),
        _ => list(&devs),
    }
}

fn fail(msg: &str) -> ! {
    log::error(msg);
    std::process::exit(64);
}

fn list(devs: &[devices::CaptureDevice]) {
    for d in devs {
        print_device(d);
    }
    let elgato = devs.iter().filter(|d| d.is_elgato).count();
    if elgato > 0 {
        log::ok(&format!("found {elgato} Elgato capture device(s)"));
    } else {
        log::warn("no Elgato card found; other capture devices still work");
    }
}

fn run_finish(args: &[String]) {
    let Some(input) = args.iter().find(|a| !a.starts_with('-')) else {
        fail("finish needs a recording, e.g. lucentia finish ~/Videos/Lucentia/clip.mkv");
    };
    let mp4 = args.iter().any(|a| a == "--mp4");
    let mix = args.iter().any(|a| a == "--mix");
    if !mp4 && !mix {
        fail("finish needs --mp4, --mix or both");
    }
    let input = PathBuf::from(input);
    let out = if mp4 {
        input.with_extension("mp4")
    } else {
        input.clone()
    };
    let codec = args
        .iter()
        .position(|a| a == "--audio")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| AudioCodec::parse(v))
        .unwrap_or(AudioCodec::Aac);
    match remux::finalize(&input, &out, mp4, mix.then_some((codec, 320))) {
        Ok(()) => log::ok(&format!("saved {}", out.display())),
        Err(e) => {
            log::error(&e);
            std::process::exit(1);
        }
    }
}

fn list_mics() {
    let mics = devices::list_mics();
    if mics.is_empty() {
        log::warn("no microphones found");
        return;
    }
    println!(
        "{}",
        log::bold("Microphones (use part of the name with --mic):")
    );
    for d in mics {
        let default = d
            .properties()
            .and_then(|p| p.get::<bool>("is-default").ok())
            .unwrap_or(false);
        let tag = if default {
            log::green("  [default]")
        } else {
            String::new()
        };
        println!("  {}{tag}", d.display_name());
    }
}

fn default_capture() -> preview::Options {
    preview::Options {
        mode: None,
        hdr: false,
        mjpeg: false,
        hdr_signal: false,
        audio: true,
    }
}

fn next_value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str, hint: &str) -> &'a str {
    it.next()
        .map(String::as_str)
        .unwrap_or_else(|| fail(&format!("{flag} needs a value, e.g. {hint}")))
}

fn parse_num<T: std::str::FromStr>(v: &str, flag: &str) -> T {
    v.parse()
        .unwrap_or_else(|_| fail(&format!("{flag}: `{v}` isn't a valid number")))
}

/// Options shared by preview and record. Returns false if `a` isn't one of them.
fn capture_flag(
    a: &str,
    it: &mut std::slice::Iter<'_, String>,
    opts: &mut preview::Options,
    index: &mut usize,
) -> bool {
    match a {
        "--mode" => {
            let v = next_value(it, "--mode", "1920x1080@60");
            opts.mode =
                Some(preview::parse_mode(v).unwrap_or_else(|| {
                    fail(&format!("can't read mode `{v}`; use e.g. 1920x1080@60"))
                }));
        }
        "--hdr" => opts.hdr = true,
        "--mjpeg" => opts.mjpeg = true,
        "--hdr-signal" => opts.hdr_signal = true,
        "--no-audio" => opts.audio = false,
        "--device" => {
            *index = parse_num(next_value(it, "--device", "1"), "--device");
            if *index == 0 {
                fail("--device starts at 1");
            }
        }
        "--debug" | "-d" => {}
        _ => return false,
    }
    true
}

fn pick_card<'a>(devs: &'a [devices::CaptureDevice], index: usize) -> &'a devices::CaptureDevice {
    devs.get(index - 1)
        .unwrap_or_else(|| fail(&format!("there is no device {index}; found {}", devs.len())))
}

fn run_preview(args: &[String], devs: &[devices::CaptureDevice]) {
    let mut opts = default_capture();
    let mut index = 1usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !capture_flag(a, &mut it, &mut opts, &mut index) {
            fail(&format!("unknown preview option `{a}`\n\n{USAGE}"));
        }
    }
    if opts.hdr && opts.mjpeg {
        fail("--hdr and --mjpeg can't be combined: the card's MJPEG modes are 8-bit");
    }
    if let Err(e) = preview::run(pick_card(devs, index), &opts) {
        log::error(&e);
        std::process::exit(1);
    }
}

fn run_record(args: &[String], devs: &[devices::CaptureDevice]) {
    let mut opts = record::Options {
        capture: default_capture(),
        codec: None,
        rate: None,
        keyframe_secs: 2.0,
        analysis: false,
        audio_codec: AudioCodec::Aac,
        audio_kbps: 320,
        mic: None,
        mic_offset_ms: 0,
        mix_track: true,
        game_volume: 100.0,
        mic_volume: 100.0,
        container: record::Container::Mkv,
        output: None,
        duration: None,
        preview: false,
        test_source: false,
    };
    let mut index = 1usize;
    let mbps = |v: &str, flag: &str| -> u32 {
        let m: f64 = parse_num(v, flag);
        if !(0.1..=2000.0).contains(&m) {
            fail(&format!("{flag}: {m} Mbit/s is out of range (0.1 to 2000)"));
        }
        (m * 1000.0).round() as u32
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if capture_flag(a, &mut it, &mut opts.capture, &mut index) {
            continue;
        }
        match a.as_str() {
            "--codec" => {
                let v = next_value(&mut it, "--codec", "hevc");
                opts.codec = Some(Codec::parse(v).unwrap_or_else(|| {
                    fail(&format!("unknown codec `{v}` (h264, hevc, av1, raw)"))
                }));
            }
            "--cbr" => {
                opts.rate = Some(RateControl::Cbr {
                    kbps: mbps(next_value(&mut it, "--cbr", "40"), "--cbr"),
                })
            }
            "--vbr" => {
                let v = next_value(&mut it, "--vbr", "40:60");
                let (avg, max) = match v.split_once(':') {
                    Some((a, m)) => (mbps(a, "--vbr"), mbps(m, "--vbr")),
                    None => {
                        let a = mbps(v, "--vbr");
                        (a, a * 3 / 2)
                    }
                };
                if max < avg {
                    fail("--vbr: the ceiling has to be at least the average");
                }
                opts.rate = Some(RateControl::Vbr {
                    kbps: avg,
                    max_kbps: max,
                });
            }
            "--quality" => {
                let q: u32 = parse_num(next_value(&mut it, "--quality", "20"), "--quality");
                if q > 51 {
                    fail("--quality goes from 0 (best) to 51 (worst)");
                }
                opts.rate = Some(RateControl::Quality { q });
            }
            "--keyframe" => {
                opts.keyframe_secs =
                    parse_num(next_value(&mut it, "--keyframe", "2"), "--keyframe");
                if opts.keyframe_secs <= 0.0 {
                    fail("--keyframe has to be more than 0 seconds");
                }
            }
            "--analysis" => opts.analysis = true,
            "--audio" => {
                let v = next_value(&mut it, "--audio", "aac");
                opts.audio_codec = AudioCodec::parse(v).unwrap_or_else(|| {
                    fail(&format!("unknown audio codec `{v}` (aac, opus, pcm)"))
                });
            }
            "--audio-bitrate" => {
                opts.audio_kbps = parse_num(
                    next_value(&mut it, "--audio-bitrate", "320"),
                    "--audio-bitrate",
                )
            }
            "--mic" => opts.mic = Some(next_value(&mut it, "--mic", "Wave").to_string()),
            "--mic-offset" => {
                opts.mic_offset_ms =
                    parse_num(next_value(&mut it, "--mic-offset", "40"), "--mic-offset")
            }
            "--mix-track" => opts.mix_track = true,
            "--no-mix-track" => opts.mix_track = false,
            "--game-volume" => {
                opts.game_volume =
                    parse_num(next_value(&mut it, "--game-volume", "80"), "--game-volume")
            }
            "--mic-volume" => {
                opts.mic_volume =
                    parse_num(next_value(&mut it, "--mic-volume", "60"), "--mic-volume")
            }
            "--format" => {
                opts.container = match next_value(&mut it, "--format", "mkv")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "mp4" => record::Container::Mp4,
                    "mkv" => record::Container::Mkv,
                    other => fail(&format!("unknown format `{other}` (mp4, mkv)")),
                }
            }
            "--output" | "-o" => {
                opts.output = Some(PathBuf::from(next_value(&mut it, "--output", "clip.mp4")))
            }
            "--duration" => {
                opts.duration = Some(parse_num(
                    next_value(&mut it, "--duration", "60"),
                    "--duration",
                ))
            }
            "--preview" => opts.preview = true,
            "--test-source" => opts.test_source = true,
            other => fail(&format!("unknown record option `{other}`\n\n{USAGE}")),
        }
    }
    if opts.capture.hdr && opts.capture.mjpeg {
        fail("--hdr and --mjpeg can't be combined: the card's MJPEG modes are 8-bit");
    }
    let card = if opts.test_source {
        None
    } else {
        Some(pick_card(devs, index))
    };
    if let Err(e) = record::run(card, &opts) {
        log::error(&e);
        std::process::exit(1);
    }
}

fn print_device(d: &devices::CaptureDevice) {
    let tag = if d.is_elgato {
        log::green(" [Elgato]")
    } else {
        String::new()
    };
    println!("\n{}{tag}", log::bold(&d.name));
    let ids = match (&d.vendor_id, &d.product_id) {
        (Some(v), Some(p)) => format!("{v}:{p}"),
        _ => "unknown".into(),
    };
    println!(
        "  {} {}   {} {}   {} {}",
        log::dim("path"),
        d.path.as_deref().unwrap_or("unknown"),
        log::dim("usb"),
        ids,
        log::dim("serial"),
        d.serial.as_deref().unwrap_or("unknown"),
    );

    if let Some(note) = d.product_id.as_deref().and_then(devices::link_speed_note) {
        log::warn(&format!("  {note}"));
    }

    if d.modes.is_empty() {
        log::warn("  this device reported no usable modes");
        return;
    }

    print_group(
        "Lossless (uncompressed)",
        d.modes.iter().filter(|m| m.kind == Kind::Lossless),
    );
    print_group(
        "MJPEG (compressed on the card)",
        d.modes.iter().filter(|m| m.kind == Kind::Mjpeg),
    );
}

/// One line per resolution: "1920x1080  240, 144, 120, 60, 59.94 ...", with 10-bit
/// capable rates marked.
fn print_group<'a>(title: &str, modes: impl Iterator<Item = &'a Mode>) {
    use std::collections::BTreeMap;
    // resolution -> (fps label -> has 10-bit)
    let mut by_res: BTreeMap<(i32, i32), BTreeMap<i64, (String, bool)>> = BTreeMap::new();
    for m in modes {
        let entry = by_res
            .entry((m.width, m.height))
            .or_default()
            .entry(-(m.fps() * 1000.0).round() as i64) // sort high to low
            .or_insert_with(|| (fps_label(m), false));
        entry.1 |= m.ten_bit();
    }
    if by_res.is_empty() {
        return;
    }
    let colored = if title.starts_with("Lossless") {
        log::green(title)
    } else {
        log::yellow(title)
    };
    println!("  {colored}");
    for ((w, h), rates) in by_res.iter().rev() {
        let list: Vec<String> = rates
            .values()
            .map(|(label, ten)| {
                if *ten {
                    format!("{label} {}", log::cyan("HDR"))
                } else {
                    label.clone()
                }
            })
            .collect();
        println!("    {:>9}  {}", format!("{w}x{h}"), list.join(", "));
    }
}

fn fps_label(m: &Mode) -> String {
    let f = m.fps();
    if (f - f.round()).abs() < 0.01 {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.2}")
    }
}
