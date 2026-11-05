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

fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| format!("GStreamer element `{factory}` is missing. Is its plugin installed?"))
}

fn mode_caps(mode: &Mode) -> gst::Caps {
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

fn video_source(card: &CaptureDevice) -> Result<gst::Element, String> {
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
            _ => " lossless",
        }
    );
    println!("{} {} at {label}", log::green("opening"), card.name);

    let pipeline = gst::Pipeline::new();

    // Video branch.
    let src = video_source(card)?;
    let caps = make("capsfilter")?;
    caps.set_property("caps", mode_caps(&mode));
    let queue = make("queue")?;
    let sink = make("autovideosink")?;
    sink.set_property("sync", false); // show frames the moment they arrive

    let mut chain = vec![src, caps.clone(), queue];
    if mode.kind == Kind::Mjpeg {
        // Hardware JPEG decoding when available (AMD/Intel via VA-API).
        let dec = make("vajpegdec").or_else(|_| make("jpegdec"))?;
        log::debug(&format!(
            "MJPEG decoder: {}",
            dec.factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default()
        ));
        chain.push(dec);
    }
    chain.push(make("videoconvert")?);
    chain.push(sink);
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
                let asrc = dev
                    .create_element(Some("audio-source"))
                    .map_err(|e| format!("could not open the card's audio: {e}"))?;
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

fn fmt_fps(f: f64) -> String {
    if (f - f.round()).abs() < 0.01 {
        format!("{}", f.round() as i64)
    } else {
        format!("{f:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        assert_eq!(parse_mode("1920x1080@60"), Some((1920, 1080, 60.0)));
        assert_eq!(parse_mode("3840x2160@59.94"), Some((3840, 2160, 59.94)));
        assert_eq!(parse_mode("1080p60"), None);
    }
}
