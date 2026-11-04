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

/// Known Elgato models by USB product ID. Anything else from Elgato still works,
/// it just shows the name the system reports.
const KNOWN_MODELS: &[(&str, &str)] = &[("009b", "Elgato 4K X")];

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
}

/// List every video capture device currently connected.
pub fn list() -> Result<Vec<CaptureDevice>, String> {
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some("Video/Source"), None);
    monitor
        .start()
        .map_err(|e| format!("could not start the device monitor: {e}"))?;
    let raw = monitor.devices();
    monitor.stop();

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
        let mut vendor = get(&["device.vendor.id", "api.v4l2.cap.vendor"]).map(norm_id);
        let mut product = get(&["device.product.id"]).map(norm_id);
        let serial = get(&["device.serial"]);

        // Some setups don't put USB IDs in the properties; read them from sysfs instead.
        if vendor.is_none() {
            if let Some(p) = &path {
                let (v, pr) = usb_ids_from_sysfs(p);
                vendor = v;
                product = product.or(pr);
            }
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

        // The same card can show up once per device provider (V4L2 and PipeWire).
        // Keep the first one per path, or per name when there's no path.
        let dup = out.iter().any(|d| match (&d.path, &path) {
            (Some(a), Some(b)) => a == b,
            _ => d.name == name && d.modes == modes,
        });
        if dup {
            log::debug(&format!("  skipping duplicate of {name}"));
            continue;
        }

        out.push(CaptureDevice {
            name,
            path,
            vendor_id: vendor,
            product_id: product,
            serial,
            is_elgato,
            modes,
        });
    }

    // Elgato cards first, then everything else.
    out.sort_by_key(|d| !d.is_elgato);
    Ok(out)
}

/// "0x0FD9" and "0fd9" both become "0fd9".
fn norm_id(s: String) -> String {
    s.trim().trim_start_matches("0x").to_lowercase()
}

#[cfg(target_os = "linux")]
fn usb_ids_from_sysfs(dev_path: &str) -> (Option<String>, Option<String>) {
    // /dev/video0 -> /sys/class/video4linux/video0/device is the USB interface;
    // the parent directory holds idVendor / idProduct.
    let Some(node) = dev_path.strip_prefix("/dev/") else {
        return (None, None);
    };
    let base = format!("/sys/class/video4linux/{node}/device/..");
    let read = |f: &str| {
        std::fs::read_to_string(format!("{base}/{f}"))
            .ok()
            .map(|s| norm_id(s))
    };
    (read("idVendor"), read("idProduct"))
}

#[cfg(not(target_os = "linux"))]
fn usb_ids_from_sysfs(_dev_path: &str) -> (Option<String>, Option<String>) {
    (None, None)
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
        let (Ok(width), Ok(height)) = (s.get::<i32>("width"), s.get::<i32>("height")) else {
            continue;
        };
        let formats: Vec<String> = match kind {
            Kind::Mjpeg => vec![String::new()],
            Kind::Lossless => string_or_list(s, "format"),
        };
        for (num, den) in framerates(s) {
            if num == 0 {
                continue;
            }
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
    set.into_iter().collect()
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
    fn normalizes_usb_ids() {
        assert_eq!(norm_id("0x0FD9".into()), "0fd9");
        assert_eq!(norm_id("009b\n".into()), "009b");
    }
}
