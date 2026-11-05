//! Live preview: Picture in a window, the card's audio on the speakers.
//!
//! The pipeline mirrors what we tested by hand with gst-launch:
//!   video: source -> caps (exact mode) -> queue -> [decode/convert] -> window
//!   audio: card's audio source -> queue -> convert -> resample -> speakers

use gst::prelude::*;
use gstreamer as gst;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::devices::{self, CaptureDevice, Kind, Mode};
use crate::log;

pub struct Options {
    /// Width, height and frame rate, e.g. 1920x1080@60. None = the card's default pick.
    pub mode: Option<(i32, i32, f64)>,
    /// Use the 10-bit (P010) variant of the mode.
    pub hdr: bool,
    /// Use the card's MJPEG variant of the mode.
    pub mjpeg: bool,
    /// The source sends HDR even though the mode is 8-bit (tone map the preview).
    pub hdr_signal: bool,
    pub audio: bool,
}

/// Parse "1920x1080@60" or "1920x1080@59.94".
pub fn parse_mode(s: &str) -> Option<(i32, i32, f64)> {
    let (size, fps) = s.split_once('@')?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, fps.parse().ok()?))
}

/// Pick the mode to open: the requested one if the card supports it, otherwise
/// 1080p60 lossless, otherwise the first lossless mode it has.
pub fn choose_mode(card: &CaptureDevice, opts: &Options) -> Result<Mode, String> {
    let kind = if opts.mjpeg {
        Kind::Mjpeg
    } else {
        Kind::Lossless
    };
    let matches = |m: &&Mode, w: i32, h: i32, fps: f64| {
        m.kind == kind
            && m.width == w
            && m.height == h
            && (m.fps() - fps).abs() < 0.01
            && (kind == Kind::Mjpeg || m.ten_bit() == opts.hdr)
    };
    // Prefer NV12 among the 8-bit formats: it's what the GPU encoders take directly.
    let rank = |m: &&Mode| match m.format.as_str() {
        "NV12" | "P010_10LE" | "" => 0,
        "YUY2" => 1,
        _ => 2,
    };

    let (w, h, fps) = opts.mode.unwrap_or((1920, 1080, 60.0));
    if let Some(m) = card
        .modes
        .iter()
        .filter(|m| matches(m, w, h, fps))
        .min_by_key(rank)
    {
        return Ok(m.clone());
    }
    if opts.mode.is_some() {
        let what = if opts.mjpeg {
            "as MJPEG"
        } else if opts.hdr {
            "in 10-bit HDR"
        } else {
            "lossless"
        };
        return Err(format!(
            "{} can't do {w}x{h}@{fps} {what}. Run `lucentia` to see the modes it supports.",
            card.name
        ));
    }
    card.modes
        .iter()
        .filter(|m| m.kind == kind)
        .min_by_key(rank)
        .cloned()
        .ok_or_else(|| format!("{} reported no usable modes", card.name))
}

pub(crate) fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| format!("GStreamer element `{factory}` is missing. Is its plugin installed?"))
}

pub(crate) fn mode_caps(mode: &Mode) -> gst::Caps {
    let name = match mode.kind {
        Kind::Lossless => "video/x-raw",
        Kind::Mjpeg => "image/jpeg",
    };
    let mut b = gst::Caps::builder(name)
        .field("width", mode.width)
        .field("height", mode.height)
        .field("framerate", gst::Fraction::new(mode.fps_num, mode.fps_den));
    if mode.kind == Kind::Lossless {
        b = b.field("format", mode.format.as_str());
    }
    b.build()
}

pub(crate) fn video_source(card: &CaptureDevice) -> Result<gst::Element, String> {
    // On Linux, open the card through V4L2 directly: PipeWire's camera path hides the
    // 10-bit modes.
    if cfg!(target_os = "linux") {
        if let Some(path) = &card.path {
            let src = make("v4l2src")?;
            src.set_property("device", path);
            return Ok(src);
        }
    }
    card.device
        .create_element(Some("video-source"))
        .map_err(|e| format!("could not open {}: {e}", card.name))
}

/// Open a capture card's audio.
///
/// On Linux we go through PipeWire's PulseAudio layer (pulsesrc) by node name. That's
/// the path that played cleanly in our tests; pipewiresrc dropped out after a moment.
pub(crate) fn audio_source(dev: &gst::Device) -> Result<gst::Element, String> {
    if cfg!(target_os = "linux") {
        let name = dev.properties().and_then(|p| {
            ["node.name", "device.name"]
                .iter()
                .find_map(|k| p.get::<String>(*k).ok())
        });
        if let (Some(name), Ok(src)) = (name, make("pulsesrc")) {
            log::debug(&format!("audio: pulsesrc device={name}"));
            src.set_property("device", &name);
            // Each audio device runs on its own clock. The default way of keeping up
            // ("skew") skips or repeats chunks when clocks drift, which sounds choppy.
            // Resampling by a tiny amount instead is inaudible.
            if src.find_property("slave-method").is_some() {
                src.set_property_from_str("slave-method", "resample");
            }
            return Ok(src);
        }
        log::debug("audio: no node name or pulsesrc, using the device's own element");
    }
    dev.create_element(Some("audio-source"))
        .map_err(|e| format!("could not open the card's audio: {e}"))
}

/// HDR10 to SDR tone mapping for the preview, run on the GPU by glshader.
///
/// glcolorconvert has already turned the frame into RGB using the BT.2020 matrix, but
/// the values are still PQ-encoded. This decodes PQ to light levels, compresses
/// highlights from ~1000 nits down to SDR (diffuse white at 203 nits, per BT.2408),
/// converts BT.2020 colors to BT.709, and encodes for an sRGB screen.
const TONEMAP_SHADER: &str = r#"
#ifdef GL_ES
precision highp float;
#endif
varying vec2 v_texcoord;
uniform sampler2D tex;

vec3 pq_to_nits(vec3 e) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    vec3 p = pow(max(e, 0.0), vec3(1.0 / m2));
    return 10000.0 * pow(max(p - c1, 0.0) / (c2 - c3 * p), vec3(1.0 / m1));
}

void main() {
    vec4 px = texture2D(tex, v_texcoord);
    // Light relative to SDR white (1.0 = 203 nits).
    vec3 rgb = pq_to_nits(px.rgb) / 203.0;

    // Extended Reinhard on luminance, so colors keep their hue.
    float peak = 1000.0 / 203.0;
    float y = dot(rgb, vec3(0.2627, 0.6780, 0.0593));
    float mapped = y * (1.0 + y / (peak * peak)) / (1.0 + y);
    rgb *= (y > 0.0) ? mapped / y : 0.0;

    // BT.2020 primaries to BT.709.
    rgb = vec3(
        dot(rgb, vec3( 1.6605, -0.5876, -0.0728)),
        dot(rgb, vec3(-0.1246,  1.1329, -0.0083)),
        dot(rgb, vec3(-0.0182, -0.1006,  1.1187)));
    rgb = clamp(rgb, 0.0, 1.0);

    // sRGB encoding.
    vec3 lo = rgb * 12.92;
    vec3 hi = 1.055 * pow(rgb, vec3(1.0 / 2.4)) - 0.055;
    rgb = mix(lo, hi, step(vec3(0.0031308), rgb));

    gl_FragColor = vec4(rgb, px.a);
}
"#;

/// Whether frames should be treated as HDR (BT.2100 PQ): every 10-bit mode, or an
/// 8-bit mode when the user says the source is sending HDR (4K60 HDR over 10 Gbps).
pub(crate) fn is_hdr(mode: &Mode, hdr_signal: bool) -> bool {
    mode.ten_bit() || hdr_signal
}

/// Card source -> exact mode -> [HDR tag] -> [MJPEG decode]. Returns the elements and
/// the capsfilter, whose src pad sees every frame exactly as it left the card.
pub(crate) fn capture_chain(
    card: &CaptureDevice,
    mode: &Mode,
    hdr: bool,
) -> Result<(Vec<gst::Element>, gst::Element), String> {
    let src = video_source(card)?;
    let caps = make("capsfilter")?;
    caps.set_property("caps", mode_caps(mode));
    let mut chain = vec![src, caps.clone()];

    if mode.kind == Kind::Mjpeg {
        // Hardware JPEG decoding when available (AMD/Intel via VA-API).
        match make("vajpegdec") {
            Ok(dec) => {
                log::debug("MJPEG decoder: vajpegdec (GPU)");
                chain.push(dec);
                chain.push(make("vapostproc")?);
            }
            Err(_) => {
                log::warn("no GPU JPEG decoder (vajpegdec), decoding MJPEG on the CPU");
                chain.push(make("jpegdec")?);
            }
        }
    }
    if hdr {
        // The card doesn't report its signal as HDR, so say so ourselves. The GPU color
        // conversion and the encoders read this tag.
        let tag = make("capssetter")?;
        tag.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("colorimetry", "bt2100-pq")
                .build(),
        );
        chain.push(tag);
    }
    Ok((chain, caps))
}

/// GPU display: upload, convert, [tone map HDR], window. Falls back to the CPU.
pub(crate) fn display_chain(hdr: bool) -> Result<Vec<gst::Element>, String> {
    let mut chain = Vec::new();
    match (
        make("glupload"),
        make("glcolorconvert"),
        make("glimagesink"),
    ) {
        (Ok(up), Ok(conv), Ok(sink)) => {
            chain.push(up);
            chain.push(conv);
            if hdr {
                let tm = make("glshader")?;
                tm.set_property("fragment", TONEMAP_SHADER);
                chain.push(tm);
            }
            sink.set_property("sync", false); // show frames the moment they arrive
            chain.push(sink);
        }
        _ => {
            log::warn("OpenGL elements missing, falling back to a slower CPU preview");
            if hdr {
                log::warn("HDR will look washed out without the OpenGL tone mapping");
            }
            chain.push(make("videoconvert")?);
            let sink = make("autovideosink")?;
            sink.set_property("sync", false);
            chain.push(sink);
        }
    }
    Ok(chain)
}

/// A preview frame for the app window: tightly packed RGBA, already tone-mapped.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Where preview frames go when the app window is showing them.
pub type FrameSink = std::sync::Arc<dyn Fn(Frame) + Send + Sync>;

/// Preview frames for the app window: convert, tone map HDR and shrink on the GPU,
/// then hand small RGBA frames (e.g. 1280x720) to `sink`. Only the preview is scaled;
/// recordings keep full resolution. Falls back to the CPU without OpenGL.
pub(crate) fn frames_chain(
    hdr: bool,
    width: u32,
    height: u32,
    sink: FrameSink,
) -> Result<Vec<gst::Element>, String> {
    let rgba = |gl: bool| {
        let b = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", width as i32)
            .field("height", height as i32);
        if gl {
            b.features(["memory:GLMemory"]).build()
        } else {
            b.build()
        }
    };
    let mut chain = Vec::new();
    match (
        make("glupload"),
        make("glcolorconvert"),
        make("glcolorscale"),
        make("gldownload"),
    ) {
        (Ok(up), Ok(conv), Ok(scale), Ok(down)) => {
            chain.push(up);
            chain.push(conv);
            if hdr {
                let tm = make("glshader")?;
                tm.set_property("fragment", TONEMAP_SHADER);
                chain.push(tm);
            }
            chain.push(scale);
            let gl_caps = make("capsfilter")?;
            gl_caps.set_property("name", "preview-gl-size");
            gl_caps.set_property("caps", rgba(true));
            chain.push(gl_caps);
            chain.push(down);
        }
        _ => {
            log::warn("OpenGL elements missing, scaling the preview on the CPU");
            chain.push(make("videoconvert")?);
            chain.push(make("videoscale")?);
        }
    }
    let caps = make("capsfilter")?;
    caps.set_property("name", "preview-size");
    caps.set_property("caps", rgba(false));
    chain.push(caps);

    let appsink = gstreamer_app::AppSink::builder()
        .sync(false)
        .max_buffers(1)
        .drop(true)
        .build();
    appsink.set_callbacks(
        gstreamer_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(frame) = sample_to_frame(&sample) {
                    sink(frame);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    chain.push(appsink.upcast());
    Ok(chain)
}

/// Copy a sample into a tightly packed RGBA frame, dropping any row padding.
fn sample_to_frame(sample: &gst::Sample) -> Option<Frame> {
    use gstreamer_video::prelude::*;
    let caps = sample.caps()?;
    let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
    let buffer = sample.buffer()?;
    let frame = gstreamer_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (w, h) = (info.width() as usize, info.height() as usize);
    let stride = frame.plane_stride()[0] as usize;
    let data = frame.plane_data(0).ok()?;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        rgba.extend_from_slice(&data[row * stride..row * stride + w * 4]);
    }
    Some(Frame {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

/// Preview size for a mode: 1280 wide (or the mode's own width if smaller), keeping
/// the aspect ratio, rounded to even numbers.
pub fn preview_size(mode: &Mode) -> (u32, u32) {
    preview_size_max(mode, 1280)
}

/// Preview frame size for a mode, at most `max_width` wide, keeping the shape.
pub fn preview_size_max(mode: &Mode, max_width: i32) -> (u32, u32) {
    let w = mode.width.min(max_width).max(2) as u32;
    let h = ((w as f64 * mode.height as f64 / mode.width.max(1) as f64).round() as u32).max(2);
    (w & !1, h & !1)
}

pub fn run(card: &CaptureDevice, opts: &Options) -> Result<(), String> {
    let mode = choose_mode(card, opts)?;
    let label = format!(
        "{}x{}@{}{}",
        mode.width,
        mode.height,
        fmt_fps(mode.fps()),
        match (&mode.kind, mode.ten_bit()) {
            (Kind::Mjpeg, _) => " MJPEG",
            (_, true) => " 10-bit HDR",
            _ if opts.hdr_signal => " 8-bit HDR",
            _ => " lossless",
        }
    );
    println!("{} {} at {label}", log::green("opening"), card.name);

    let pipeline = gst::Pipeline::new();

    // Video branch. Everything after the source runs on the GPU through OpenGL:
    // converting 4K60 frames on the CPU is what made the first version lag.
    let hdr = is_hdr(&mode, opts.hdr_signal);
    let (mut chain, caps) = capture_chain(card, &mode, hdr)?;
    chain.push(make("queue")?);
    chain.extend(display_chain(hdr)?);
    pipeline
        .add_many(&chain)
        .map_err(|e| format!("could not build the video pipeline: {e}"))?;
    gst::Element::link_many(&chain).map_err(|_| {
        "could not connect the video pipeline (the card may have refused this mode)".to_string()
    })?;

    // Count frames coming off the card, to show the real frame rate.
    let frames = Arc::new(AtomicU64::new(0));
    {
        let frames = frames.clone();
        caps.static_pad("src")
            .ok_or("capsfilter has no src pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                frames.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
    }

    // Audio branch.
    if opts.audio {
        match devices::find_audio_for(card) {
            Some(dev) => {
                println!("{} audio from {}", log::green("found"), dev.display_name());
                let asrc = audio_source(&dev)?;
                let achain = [
                    asrc,
                    make("queue")?,
                    make("audioconvert")?,
                    make("audioresample")?,
                    make("autoaudiosink")?,
                ];
                pipeline
                    .add_many(&achain)
                    .map_err(|e| format!("could not build the audio pipeline: {e}"))?;
                gst::Element::link_many(&achain)
                    .map_err(|_| "could not connect the audio pipeline".to_string())?;
            }
            None => log::warn(
                "couldn't find this card's audio, previewing without sound. \
                 Run with --debug to see the audio devices that were checked.",
            ),
        }
    }

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| "the pipeline failed to start. Run with --debug for details.".to_string())?;
    println!(
        "{} close the window or press Ctrl+C to stop",
        log::dim("preview running:")
    );

    let result = watch(&pipeline, &frames, mode.fps());
    let _ = pipeline.set_state(gst::State::Null);
    result
}

/// Wait for the preview to end, printing the frame rate every 5 seconds.
fn watch(pipeline: &gst::Pipeline, frames: &AtomicU64, target: f64) -> Result<(), String> {
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let mut last = (Instant::now(), 0u64);
    loop {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(500)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => return Ok(()),
                MessageView::Error(e) => {
                    let text = e.error().to_string();
                    // Closing the preview window is a normal way to stop.
                    if text.contains("window was closed") {
                        println!("{}", log::green("preview closed"));
                        return Ok(());
                    }
                    let debug = e.debug().map(|d| d.to_string()).unwrap_or_default();
                    log::debug(&format!(
                        "from {:?}: {debug}",
                        e.src().map(|s| s.path_string())
                    ));
                    return Err(text);
                }
                MessageView::Warning(w) => log::warn(&w.error().to_string()),
                _ => {}
            }
        }

        let elapsed = last.0.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let now = frames.load(Ordering::Relaxed);
            let fps = (now - last.1) as f64 / elapsed.as_secs_f64();
            let line = format!("{fps:.2} fps (target {})", fmt_fps(target));
            if fps < target - 0.5 {
                log::warn(&format!("{line}: frames are being lost"));
            } else {
                println!("  {line}");
            }
            last = (Instant::now(), now);
        }
    }
}

pub(crate) fn fmt_fps(f: f64) -> String {
    if (f - f.round()).abs() < 0.01 {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a 10-bit test pattern through the same HDR chain the preview uses
    /// (BT.2100 PQ tag, GL upload and conversion, tone-mapping shader). Needs a
    /// working OpenGL driver, so it's opt-in: `cargo test -- --ignored`.
    /// Set LUCENTIA_TONEMAP_PNG=/path/out.png to save the result for a visual check.
    #[test]
    #[ignore]
    fn hdr_tonemap_shader_runs() {
        gst::init().unwrap();
        let out = std::env::var("LUCENTIA_TONEMAP_PNG").ok();
        let tail = match &out {
            Some(path) => format!(
                "gldownload ! video/x-raw,format=RGBA ! pngenc snapshot=true ! filesink location={path}"
            ),
            None => "fakesink".to_string(),
        };
        let pipeline = gst::parse::launch(&format!(
            "videotestsrc num-buffers=5 pattern=smpte ! \
             video/x-raw,format=P010_10LE,width=640,height=360 ! \
             capssetter caps=\"video/x-raw,colorimetry=bt2100-pq\" ! \
             glupload ! glcolorconvert ! glshader name=tm ! {tail}"
        ))
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        pipeline
            .by_name("tm")
            .unwrap()
            .set_property("fragment", TONEMAP_SHADER);
        pipeline.set_state(gst::State::Playing).unwrap();
        let msg = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                gst::ClockTime::from_seconds(60),
                &[gst::MessageType::Error, gst::MessageType::Eos],
            )
            .expect("timed out");
        pipeline.set_state(gst::State::Null).unwrap();
        if let gst::MessageView::Error(e) = msg.view() {
            panic!("{}: {:?}", e.error(), e.debug());
        }
    }

    #[test]
    fn parses_modes() {
        assert_eq!(parse_mode("1920x1080@60"), Some((1920, 1080, 60.0)));
        assert_eq!(parse_mode("3840x2160@59.94"), Some((3840, 2160, 59.94)));
        assert_eq!(parse_mode("1080p60"), None);
    }
}
