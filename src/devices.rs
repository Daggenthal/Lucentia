//! Finding capture devices and the modes they support.
//!
//! GStreamer's DeviceMonitor lists video sources on Linux, Windows and macOS. For each
//! one we work out whether it's an Elgato card (USB vendor ID 0fd9) and turn its caps
//! into a plain list of modes the UI can offer.

use gst::prelude::*;
use gstreamer as gst;
use std::collections::BTreeSet;

use crate::log;

/// Elgato's USB vendor ID. The Wave XLR shares it, so we also require video.
const ELGATO_VENDOR: &str = "0fd9";

/// Known Elgato capture cards by USB product ID. Anything else from Elgato still
/// works, it just shows the name the system reports. Only the 4K X is tested.
///
/// The 4K X and 4K S change their product ID with the USB link speed, so a card
/// on a slower port shows up under a different ID (and offers fewer modes).
const KNOWN_MODELS: &[(&str, &str)] = &[
    ("009b", "Elgato 4K X"), // 10 Gbps
    ("009c", "Elgato 4K X"), // 5 Gbps
    ("009d", "Elgato 4K X"), // USB 2.0
    ("00af", "Elgato 4K S"), // USB 3
    ("00ae", "Elgato 4K S"), // USB 2.0
    ("0082", "Elgato HD60 X"),
    ("0066", "Elgato Cam Link 4K"),
];

/// A note to show when a card is running in a reduced mode, based on its product ID.
pub fn link_speed_note(product_id: &str) -> Option<&'static str> {
    match product_id {
        "009c" => Some(
            "running at 5 Gbps: the fastest modes need a 10 Gbps USB-C port (USB 3.2 Gen 2)",
        ),
        "009d" | "00ae" => Some(
            "running at USB 2.0 speed: plug it into a USB 3 port for full resolution and frame rate",
        ),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Uncompressed frames straight from the card.
    Lossless,
    /// Compressed on the card before it reaches us.
    Mjpeg,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mode {
    pub kind: Kind,
    pub width: i32,
    pub height: i32,
    /// Frame rate as numerator / denominator, e.g. 60000/1001 for 59.94.
    pub fps_num: i32,
    pub fps_den: i32,
    /// Pixel format as GStreamer names it (NV12, P010_10LE, ...). Empty for MJPEG.
    pub format: String,
}

impl Mode {
    pub fn fps(&self) -> f64 {
        self.fps_num as f64 / self.fps_den.max(1) as f64
    }

    pub fn ten_bit(&self) -> bool {
        self.format.contains("10")
    }
}

#[derive(Debug, Clone)]
pub struct CaptureDevice {
    pub name: String,
    /// /dev/videoN on Linux, when known.
    pub path: Option<String>,
    pub vendor_id: Option<String>,
    pub product_id: Option<String>,
    pub serial: Option<String>,
    pub is_elgato: bool,
    pub modes: Vec<Mode>,
    /// The GStreamer device, used to create a source element on non-Linux systems.
    pub device: gst::Device,
}

/// List every video capture device currently connected.
pub fn list() -> Result<Vec<CaptureDevice>, String> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    // On Linux the PipeWire provider hides the plain V4L2 one by default, but PipeWire
    // doesn't pass every format through (the 4K X loses its 10-bit P010 modes).
    // Show both and keep the richer copy of each device below.
    monitor.set_show_all_devices(true);
    let raw = match monitor.start() {
        Ok(()) => {
            let d = monitor.devices();
            monitor.stop();
            d
        }
        // One provider failing (e.g. PipeWire not running) stops the whole monitor.
        // Fall back to asking V4L2 directly.
        Err(e) => {
            log::debug(&format!("device monitor failed ({e}), asking V4L2 directly"));
            let provider = gst::DeviceProviderFactory::by_name("v4l2deviceprovider")
                .ok_or("could not start the device monitor (no V4L2 provider)")?;
            provider
                .start()
                .map_err(|e| format!("could not list capture devices: {e}"))?;
            let d = provider.devices();
            provider.stop();
            d.into_iter().collect()
        }
    };

    let mut out: Vec<CaptureDevice> = Vec::new();
    for dev in raw {
        let props = dev.properties();
        if log::debug_on() {
            log::debug(&format!("device: {}", dev.display_name()));
            if let Some(p) = &props {
                log::debug(&format!("  properties: {p}"));
            }
            if let Some(c) = dev.caps() {
                log::debug(&format!("  caps: {c}"));
            }
        }

        let get = |keys: &[&str]| -> Option<String> {
            let p = props.as_ref()?;
            keys.iter().find_map(|k| p.get::<String>(*k).ok())
        };

        let path = get(&["api.v4l2.path", "device.path", "object.path"]);

        // Skip virtual cameras: v4l2loopback devices and Lucentia's own PipeWire
        // camera are outputs, not capture cards.
        if get(&["node.name"]).as_deref() == Some("lucentia") || is_virtual_video(path.as_deref()) {
            log::debug(&format!("  skipping virtual device {}", dev.display_name()));
            continue;
        }
        let mut vendor = get(&["device.vendor.id", "api.v4l2.cap.vendor"]).map(norm_id);
        let mut product = get(&["device.product.id"]).map(norm_id);
        let mut serial = get(&["device.serial"]);

        // Fill in anything the properties left out from sysfs.
        if let Some(p) = &path {
            let (v, pr, se) = usb_ids_from_sysfs(p);
            vendor = vendor.or(v);
            product = product.or(pr);
            // sysfs has the bare serial; the udev property adds vendor and model.
            serial = se.or(serial);
        }

        let modes = dev.caps().map(|c| modes_from_caps(&c)).unwrap_or_default();
        let is_elgato = vendor.as_deref() == Some(ELGATO_VENDOR) && !modes.is_empty();

        let mut name = dev.display_name().to_string();
        if is_elgato {
            if let Some(known) = product
                .as_deref()
                .and_then(|pid| KNOWN_MODELS.iter().find(|(id, _)| *id == pid))
            {
                name = known.1.to_string();
            }
        }

        let entry = CaptureDevice {
            name,
            path,
            vendor_id: vendor,
            product_id: product,
            serial,
            is_elgato,
            modes,
            device: dev.clone(),
        };

        // The same card can show up once per device provider (V4L2 and PipeWire).
        // Keep whichever copy reports more modes.
        let existing = out.iter().position(|d| match (&d.path, &entry.path) {
            (Some(a), Some(b)) => a == b,
            _ => d.name == entry.name,
        });
        match existing {
            Some(i) if entry.modes.len() > out[i].modes.len() => {
                log::debug(&format!(
                    "  replacing earlier copy of {} ({} modes) with this one ({} modes)",
                    entry.name,
                    out[i].modes.len(),
                    entry.modes.len()
                ));
                out[i] = entry;
            }
            Some(_) => log::debug(&format!("  skipping duplicate of {}", entry.name)),
            None => out.push(entry),
        }
    }

    // Elgato cards first, then everything else.
    out.sort_by_key(|d| !d.is_elgato);
    Ok(out)
}

/// Find the audio input that belongs to a capture card.
///
/// Capture cards show up as a separate USB audio device. Its name in PipeWire or
/// PulseAudio contains the card's serial number (on SteamOS it's a loopback source
/// like alsa_loopback_device.alsa_input.usb-Elgato_Elgato_4K_X_<serial>-02...), so we
/// match on that, falling back to the card's name.
pub fn find_audio_for(card: &CaptureDevice) -> Option<gst::Device> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    if monitor.start().is_err() {
        log::warn("could not start the audio device monitor");
        return None;
    }
    let devices = monitor.devices();
    monitor.stop();

    let describe = |d: &gst::Device| {
        let props = d.properties().map(|p| p.to_string()).unwrap_or_default();
        format!("{} {}", d.display_name(), props)
    };

    let mut by_serial = None;
    let mut by_name = None;
    for d in devices.iter() {
        let text = describe(d);
        // Speaker "monitor" sources record what's playing, not an input.
        if text.contains(".monitor") || d.device_class().contains("Monitor") {
            continue;
        }
        log::debug(&format!("audio candidate: {}", d.display_name()));
        if let Some(serial) = &card.serial {
            if by_serial.is_none() && text.contains(serial.as_str()) {
                by_serial = Some(d.clone());
            }
        }
        let short_name = card.name.trim_start_matches("Elgato ").trim();
        if by_name.is_none() && d.display_name().contains(short_name) {
            by_name = Some(d.clone());
        }
    }
    by_serial.or(by_name)
}

/// Microphones and other audio inputs, without speaker "monitor" sources.
/// Whether a /dev/videoN (or "v4l2:/dev/videoN") is a virtual device, like v4l2loopback.
fn is_virtual_video(path: Option<&str>) -> bool {
    let Some(name) = path
        .and_then(|p| p.rsplit('/').next())
        .filter(|n| n.starts_with("video"))
    else {
        return false;
    };
    std::fs::canonicalize(format!("/sys/class/video4linux/{name}"))
        .is_ok_and(|p| p.to_string_lossy().contains("/devices/virtual/"))
}

pub fn list_mics() -> Vec<gst::Device> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Audio/Source"), None);
    if monitor.start().is_err() {
        log::warn("could not start the audio device monitor");
        return Vec::new();
    }
    let devices = monitor.devices();
    monitor.stop();
    devices
        .into_iter()
        .filter(|d| {
            let props = d.properties().map(|p| p.to_string()).unwrap_or_default();
            !props.contains(".monitor") && !d.device_class().contains("Monitor")
        })
        .collect()
}

/// Find a mic by (part of) its name, case-insensitive. "default" picks the system's
/// default input.
pub fn find_mic(name: &str) -> Option<gst::Device> {
    let mics = list_mics();
    if name.eq_ignore_ascii_case("default") {
        return mics.into_iter().find(|d| {
            d.properties()
                .and_then(|p| p.get::<bool>("is-default").ok())
                .unwrap_or(false)
        });
    }
    let needle = name.to_lowercase();
    mics.into_iter()
        .find(|d| d.display_name().to_lowercase().contains(&needle))
}

/// "0x0FD9" and "0fd9" both become "0fd9".
fn norm_id(s: String) -> String {
    s.trim().trim_start_matches("0x").to_lowercase()
}

#[cfg(target_os = "linux")]
type UsbIds = (Option<String>, Option<String>, Option<String>);

/// Vendor ID, product ID and serial number of the USB device behind /dev/videoN.
#[cfg(target_os = "linux")]
fn usb_ids_from_sysfs(dev_path: &str) -> UsbIds {
    // /sys/class/video4linux/video0/device is the USB interface; its parent
    // directory is the USB device, which holds idVendor, idProduct and serial.
    let Some(node) = dev_path.strip_prefix("/dev/") else {
        return (None, None, None);
    };
    let base = format!("/sys/class/video4linux/{node}/device/..");
    let read = |f: &str| {
        std::fs::read_to_string(format!("{base}/{f}"))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    (
        read("idVendor").map(norm_id),
        read("idProduct").map(norm_id),
        read("serial"),
    )
}

#[cfg(not(target_os = "linux"))]
fn usb_ids_from_sysfs(_dev_path: &str) -> UsbIds {
    (None, None, None)
}

/// Turn GStreamer caps into a sorted, de-duplicated list of modes.
pub fn modes_from_caps(caps: &gst::CapsRef) -> Vec<Mode> {
    let mut set = BTreeSet::new();
    for s in caps.iter() {
        let kind = match s.name().as_str() {
            "video/x-raw" => Kind::Lossless,
            "image/jpeg" => Kind::Mjpeg,
            other => {
                log::debug(&format!("  ignoring caps type {other}"));
                continue;
            }
        };
        // GStreamer merges caps entries that differ in one field, so width or height
        // can be a list ({ 640, 720 }) instead of a single number.
        let widths = int_values(s, "width");
        let heights = int_values(s, "height");
        if widths.is_empty() || heights.is_empty() {
            log::debug(&format!("  skipping caps entry without fixed sizes: {s}"));
            continue;
        }
        let formats: Vec<String> = match kind {
            Kind::Mjpeg => vec![String::new()],
            Kind::Lossless => string_or_list(s, "format"),
        };
        for (num, den) in framerates(s) {
            if num == 0 {
                continue;
            }
            for &width in &widths {
                for &height in &heights {
                    for f in &formats {
                        set.insert(Mode {
                            kind: kind.clone(),
                            width,
                            height,
                            fps_num: num,
                            fps_den: den,
                            format: f.clone(),
                        });
                    }
                }
            }
        }
    }
    set.into_iter().collect()
}

fn int_values(s: &gst::StructureRef, field: &str) -> Vec<i32> {
    if let Ok(v) = s.get::<i32>(field) {
        return vec![v];
    }
    if let Ok(list) = s.get::<gst::List>(field) {
        return list.iter().filter_map(|v| v.get::<i32>().ok()).collect();
    }
    Vec::new()
}

fn string_or_list(s: &gst::StructureRef, field: &str) -> Vec<String> {
    if let Ok(v) = s.get::<String>(field) {
        return vec![v];
    }
    if let Ok(list) = s.get::<gst::List>(field) {
        return list.iter().filter_map(|v| v.get::<String>().ok()).collect();
    }
    Vec::new()
}

fn framerates(s: &gst::StructureRef) -> Vec<(i32, i32)> {
    if let Ok(f) = s.get::<gst::Fraction>("framerate") {
        return vec![(f.numer(), f.denom())];
    }
    if let Ok(list) = s.get::<gst::List>("framerate") {
        return list
            .iter()
            .filter_map(|v| v.get::<gst::Fraction>().ok())
            .map(|f| (f.numer(), f.denom()))
            .collect();
    }
    if s.has_field("framerate") {
        log::warn(&format!(
            "unsupported framerate format in caps: {:?}",
            s.value("framerate").map(|v| v.type_())
        ));
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_4k_x_style_caps() {
        gst::init().unwrap();
        // Shaped like what v4l2src reports for the 4K X.
        let caps: gst::Caps = "video/x-raw, format=(string)NV12, width=(int)1920, height=(int)1080, \
            framerate=(fraction){ 240/1, 144/1, 60/1, 60000/1001 }; \
            video/x-raw, format=(string)P010_10LE, width=(int)1920, height=(int)1080, framerate=(fraction)60/1; \
            image/jpeg, width=(int)3840, height=(int)2160, framerate=(fraction){ 144/1, 60/1 }"
            .parse()
            .unwrap();
        let modes = modes_from_caps(&caps);
        assert_eq!(modes.iter().filter(|m| m.kind == Kind::Lossless).count(), 5);
        assert_eq!(modes.iter().filter(|m| m.kind == Kind::Mjpeg).count(), 2);
        assert!(modes.iter().any(|m| m.ten_bit() && m.fps_num == 60));
        assert!(modes.iter().any(|m| (m.fps() - 59.94).abs() < 0.01));
    }

    #[test]
    fn expands_merged_sizes() {
        gst::init().unwrap();
        let caps: gst::Caps = "video/x-raw, format=(string)NV12, width=(int){ 640, 720 }, \
            height=(int)480, framerate=(fraction){ 60/1, 30/1 }"
            .parse()
            .unwrap();
        let modes = modes_from_caps(&caps);
        assert_eq!(modes.len(), 4);
        assert!(modes.iter().any(|m| m.width == 640));
        assert!(modes.iter().any(|m| m.width == 720));
    }

    #[test]
    fn normalizes_usb_ids() {
        assert_eq!(norm_id("0x0FD9".into()), "0fd9");
        assert_eq!(norm_id("009b\n".into()), "009b");
    }
}
