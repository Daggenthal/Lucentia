//! Lucentia: Capture app for Elgato (and other UVC) capture cards.
//!
//! Milestone 1: Find capture devices and list every mode they support.
//! Run with --debug (or LUCENTIA_DEBUG=1) to see everything GStreamer reports.

mod devices;
mod log;

use devices::{Kind, Mode};
use gstreamer as gst;

fn main() {
    let debug = std::env::args().any(|a| a == "--debug" || a == "-d")
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

    for d in &devs {
        print_device(d);
    }

    let elgato = devs.iter().filter(|d| d.is_elgato).count();
    if elgato > 0 {
        log::ok(&format!("found {elgato} Elgato capture device(s)"));
    } else {
        log::warn("no Elgato card found; other capture devices still work");
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
