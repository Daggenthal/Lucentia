//! Startup check: which GStreamer pieces are present. A missing plugin otherwise
//! shows up much later as a vague failure, so say up front what's missing, in words.

use gstreamer as gst;

pub struct Report {
    /// Without these, Lucentia can't preview or record at all.
    pub missing_required: Vec<&'static str>,
    /// Features that won't be available, in plain words.
    pub notes: Vec<String>,
    /// GPU (VA-API) encoders that work on this machine.
    pub gpu_encoders: Vec<&'static str>,
}

const REQUIRED: &[(&str, &str)] = &[
    ("v4l2src", "video4linux2 (capture card input)"),
    ("pulsesrc", "pulseaudio (game audio and mic)"),
    ("tee", "coreelements"),
    ("appsink", "app (preview in the window)"),
    ("glupload", "opengl (GPU preview)"),
    ("videoconvert", "videoconvertscale"),
    ("audioconvert", "audioconvert"),
    ("audioresample", "audioresample"),
    ("volume", "volume"),
    ("matroskamux", "matroska (MKV files)"),
    ("matroskademux", "matroska (MKV files)"),
    ("audiomixer", "audiomixer (Game + Mic track)"),
    ("decodebin", "playback (Game + Mic track)"),
    ("x264enc", "x264 (H.264 on the CPU)"),
];

const GPU: &[(&str, &str)] = &[
    ("vah264enc", "H.264"),
    ("vah265enc", "HEVC"),
    ("vaav1enc", "AV1"),
];

fn has(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

pub fn run() -> Report {
    let missing_required = REQUIRED
        .iter()
        .filter(|(e, _)| !has(e))
        .map(|(_, what)| *what)
        .collect();
    let gpu_encoders: Vec<&str> = GPU
        .iter()
        .filter(|(e, _)| has(e))
        .map(|(_, c)| *c)
        .collect();

    let mut notes = Vec::new();
    if gpu_encoders.is_empty() {
        notes.push(
            "No GPU encoder found (VA-API), so recording uses the CPU. On AMD or Intel, \
             install your distro's VA-API driver (mesa-va-drivers, intel-media-driver)."
                .to_string(),
        );
    }
    if !gpu_encoders.contains(&"HEVC") && !has("x265enc") {
        notes.push("HEVC isn't available (no GPU HEVC encoder and no x265).".into());
    }
    if !gpu_encoders.contains(&"AV1") && !has("svtav1enc") {
        notes.push("AV1 isn't available (no GPU AV1 encoder and no SVT-AV1).".into());
    }
    if !has("avenc_aac") {
        notes.push("AAC audio isn't available (gst-libav missing); use Opus or PCM.".into());
    }
    if !has("mp4mux") {
        notes.push("MP4 output isn't available (isomp4 plugin missing); MKV still works.".into());
    }
    Report {
        missing_required,
        notes,
        gpu_encoders,
    }
}

/// One line for the app's message bar, or None when everything is in place.
pub fn summary(r: &Report) -> Option<(String, bool)> {
    if !r.missing_required.is_empty() {
        return Some((
            format!(
                "Missing GStreamer plugins: {}. Run `lucentia check` for details.",
                r.missing_required.join(", ")
            ),
            true,
        ));
    }
    r.notes.first().map(|n| (n.clone(), false))
}

/// `lucentia check`: print everything, colored.
pub fn print() -> bool {
    use crate::log;
    let r = run();
    println!("{} {}", log::bold("GStreamer:"), gst::version_string());
    if r.missing_required.is_empty() {
        log::ok("all required plugins found");
    } else {
        for m in &r.missing_required {
            log::error(&format!("missing: {m}"));
        }
    }
    if let Some(g) = gpu_name() {
        println!("{} {g}", log::bold("GPU:"));
    }
    if r.gpu_encoders.is_empty() {
        log::warn("GPU encoding: none");
    } else {
        log::ok(&format!("GPU encoding: {}", r.gpu_encoders.join(", ")));
    }
    for n in &r.notes {
        log::warn(n);
    }
    r.missing_required.is_empty()
}

/// How a codec can be recorded on this machine.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Support {
    /// On the GPU (VA-API).
    Gpu,
    /// Only on the CPU: works, but heavy at high resolutions and frame rates.
    Cpu,
    /// Not at all.
    None,
}

/// Which way a codec would be encoded, for 8-bit or 10-bit (HDR) video.
pub fn codec_support(codec: crate::encode::Codec, ten_bit: bool) -> Support {
    let Some((hw, sw)) = crate::encode::encoders(codec) else {
        return Support::Gpu; // raw: nothing to encode
    };
    if has(hw) && (!ten_bit || crate::encode::gpu_supports_10bit(hw)) {
        Support::Gpu
    } else if has(sw) {
        Support::Cpu
    } else {
        Support::None
    }
}

/// The GPU and its video driver, as the driver describes itself, e.g.
/// "Mesa Gallium driver 25.0.1 for AMD Radeon 780M". Asked once, through libva.
pub fn gpu_name() -> Option<String> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let name = va_vendor_string().or_else(drm_description);
        if let Some(n) = &name {
            crate::log::debug(&format!("GPU: {n}"));
        }
        name
    })
    .clone()
}

/// libva's vendor string for the first render node. libva is loaded at runtime, so
/// Lucentia still starts on machines without it.
#[cfg(target_os = "linux")]
fn va_vendor_string() -> Option<String> {
    use std::ffi::{c_char, c_int, c_void, CStr};
    use std::os::fd::AsRawFd;

    let node = render_node()?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&node)
        .ok()?;
    // SAFETY: plain dlopen/dlsym of libva's public C API, called with the
    // signatures from va.h / va_drm.h. The display is terminated before returning
    // and the file stays open until then.
    unsafe {
        let va = libc::dlopen(c"libva.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        let va_drm = libc::dlopen(c"libva-drm.so.2".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if va.is_null() || va_drm.is_null() {
            return None;
        }
        let get_display = libc::dlsym(va_drm, c"vaGetDisplayDRM".as_ptr());
        let initialize = libc::dlsym(va, c"vaInitialize".as_ptr());
        let vendor = libc::dlsym(va, c"vaQueryVendorString".as_ptr());
        let terminate = libc::dlsym(va, c"vaTerminate".as_ptr());
        if get_display.is_null() || initialize.is_null() || vendor.is_null() || terminate.is_null()
        {
            return None;
        }
        type GetDisplay = unsafe extern "C" fn(c_int) -> *mut c_void;
        type Initialize = unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> c_int;
        type Vendor = unsafe extern "C" fn(*mut c_void) -> *const c_char;
        type Terminate = unsafe extern "C" fn(*mut c_void) -> c_int;
        let get_display: GetDisplay = std::mem::transmute(get_display);
        let initialize: Initialize = std::mem::transmute(initialize);
        let vendor: Vendor = std::mem::transmute(vendor);
        let terminate: Terminate = std::mem::transmute(terminate);

        let dpy = get_display(file.as_raw_fd());
        if dpy.is_null() {
            return None;
        }
        let (mut major, mut minor) = (0, 0);
        let name = if initialize(dpy, &mut major, &mut minor) == 0 {
            let s = vendor(dpy);
            (!s.is_null()).then(|| CStr::from_ptr(s).to_string_lossy().trim().to_string())
        } else {
            None
        };
        terminate(dpy);
        name.filter(|n| !n.is_empty())
    }
}

#[cfg(not(target_os = "linux"))]
fn va_vendor_string() -> Option<String> {
    None
}

/// The first GPU render node, e.g. /dev/dri/renderD128.
fn render_node() -> Option<std::path::PathBuf> {
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD")))
        .collect();
    nodes.sort();
    nodes.into_iter().next()
}

/// Without a VA-API driver: at least name the vendor and kernel driver.
fn drm_description() -> Option<String> {
    let node = render_node()?;
    let dev = std::path::Path::new("/sys/class/drm")
        .join(node.file_name()?)
        .join("device");
    let vendor = std::fs::read_to_string(dev.join("vendor")).ok()?;
    let vendor = match vendor.trim() {
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        "0x10de" => "NVIDIA",
        other => other,
    }
    .to_string();
    let driver = std::fs::read_link(dev.join("driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
    Some(match driver {
        Some(d) => format!("{vendor} GPU ({d} kernel driver, no VA-API video driver found)"),
        None => format!("{vendor} GPU (no VA-API video driver found)"),
    })
}

/// A short note explaining a codec that isn't on the GPU, for under the Codec menu.
pub fn codec_note(codec: crate::encode::Codec, ten_bit: bool) -> Option<(String, bool)> {
    let support = codec_support(codec, ten_bit);
    if support == Support::Gpu {
        return None;
    }
    let name = codec.name();
    let depth = if ten_bit { "10-bit " } else { "" };
    let gpu = gpu_name().unwrap_or_else(|| "no GPU video driver found".into());
    let (_, sw) = crate::encode::encoders(codec)?;
    Some(match support {
        Support::Cpu => (
            format!(
                "Your GPU can't encode {depth}{name} ({gpu}), so this uses the CPU ({sw}). \
                 Fine at 1080p; 4K or high frame rates may drop frames."
            ),
            false,
        ),
        _ => (
            format!(
                "{depth}{name} isn't available: your GPU can't encode it ({gpu}) and the \
                 CPU encoder ({sw}) isn't installed."
            ),
            true,
        ),
    })
}
