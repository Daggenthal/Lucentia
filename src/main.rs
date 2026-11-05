//! Lucentia: Capture app for Elgato (and other UVC) capture cards.
//!
//! Commands:
//!   lucentia                 list capture devices and their modes
//!   lucentia preview [opts]  live preview with sound
//! Add --debug (or LUCENTIA_DEBUG=1) to see everything GStreamer reports.

mod devices;
mod log;
mod preview;

use devices::{Kind, Mode};
use gstreamer as gst;

const USAGE: &str = "\
Usage:
  lucentia                      List capture devices and the modes they support
  lucentia preview [options]    Live preview with sound

Preview options:
  --mode WxH@FPS   Mode to open, e.g. 1920x1080@60 or 3840x2160@59.94 (default 1080p60)
  --hdr            Use the 10-bit HDR version of the mode
  --mjpeg          Use the card's MJPEG version of the mode (needed for 4K144)
  --no-audio       Picture only
  --device N       Which card to use when more than one is connected (default 1)

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

    if let Err(e) = gst::init() {
        log::error(&format!("GStreamer failed to start: {e}"));
        std::process::exit(1);
    }
    log::debug(&format!("GStreamer {}", gst::version_string()));

    let devs = match devices::list() {
        Ok(d) => d,
        Err(e) => {
            log::error(&e);
            std::process::exit(1);
        }
    };
    if devs.is_empty() {
        log::warn("no capture devices found. Is the card plugged in?");
        std::process::exit(2);
    }

    match args.first().map(String::as_str) {
        Some("preview") => run_preview(&args[1..], &devs),
        Some(cmd) if !cmd.starts_with('-') => {
            log::error(&format!("unknown command `{cmd}`\n\n{USAGE}"));
            std::process::exit(64);
        }
        _ => list(&devs),
    }
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

fn run_preview(args: &[String], devs: &[devices::CaptureDevice]) {
    let fail = |msg: &str| -> ! {
        log::error(msg);
        std::process::exit(64);
    };
    let mut opts = preview::Options {
        mode: None,
        hdr: false,
        mjpeg: false,
        audio: true,
    };
    let mut index = 1usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--mode" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| fail("--mode needs a value like 1920x1080@60"));
                opts.mode = Some(preview::parse_mode(v).unwrap_or_else(|| {
                    fail(&format!("can't read mode `{v}`; use e.g. 1920x1080@60"))
                }));
            }
            "--hdr" => opts.hdr = true,
            "--mjpeg" => opts.mjpeg = true,
            "--no-audio" => opts.audio = false,
            "--device" => {
                let v = it.next().unwrap_or_else(|| fail("--device needs a number"));
                index = v
                    .parse()
                    .ok()
                    .filter(|n| *n >= 1)
                    .unwrap_or_else(|| fail("--device needs a number, starting at 1"));
            }
            "--debug" | "-d" => {}
            other => fail(&format!("unknown preview option `{other}`\n\n{USAGE}")),
        }
    }
    if opts.hdr && opts.mjpeg {
        fail("--hdr and --mjpeg can't be combined: the card's MJPEG modes are 8-bit");
    }
    let card = devs
        .get(index - 1)
        .unwrap_or_else(|| fail(&format!("there is no device {index}; found {}", devs.len())));

    if let Err(e) = preview::run(card, &opts) {
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
