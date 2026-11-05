//! Video and audio encoders.
//!
//! The GPU encoders (VA-API on AMD and Intel) are used when present. If one is missing,
//! the software encoder for the same codec is used instead, with a warning, so a
//! recording still works (just with more CPU load).
//!
//! Known issue: GStreamer's vah265enc labels 10-bit HEVC streams as "Main" instead of
//! "Main 10". The video data is correct and most players are fine with it, but strict
//! tools may complain. AV1 10-bit doesn't have this problem. A future FFmpeg-based
//! encoder backend fixes it properly.

use gst::prelude::*;
use gstreamer as gst;

use crate::log;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
    /// Uncompressed frames straight from the card. Enormous files.
    Raw,
}

impl Codec {
    pub fn parse(s: &str) -> Option<Codec> {
        match s.to_ascii_lowercase().as_str() {
            "h264" | "avc" | "x264" => Some(Codec::H264),
            "hevc" | "h265" | "x265" => Some(Codec::Hevc),
            "av1" => Some(Codec::Av1),
            "raw" | "uncompressed" => Some(Codec::Raw),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
            Codec::Raw => "raw",
        }
    }

    pub fn supports_10bit(self) -> bool {
        !matches!(self, Codec::H264)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateControl {
    /// Fixed bitrate in kbit/s. File size is exact.
    Cbr { kbps: u32 },
    /// Average bitrate with a ceiling, both in kbit/s.
    Vbr { kbps: u32, max_kbps: u32 },
    /// Constant quality: 0 (best) to 51 (worst). Size depends on the content.
    Quality { q: u32 },
}

#[derive(Debug, Clone)]
pub struct VideoSettings {
    pub codec: Codec,
    pub rate: RateControl,
    /// Frames between keyframes. 1 = every frame is a keyframe (analysis mode).
    pub keyframe_interval: u32,
    pub ten_bit: bool,
}

/// Elements that turn raw frames into an encoded stream, in order. For raw, just the
/// conversion MKV needs.
pub fn video_chain(v: &VideoSettings) -> Result<Vec<gst::Element>, String> {
    if v.codec == Codec::Raw {
        if v.ten_bit {
            return Err(
                "MKV has no 10-bit raw format. For near-lossless 10-bit, use --analysis \
                 with a high bitrate (e.g. --codec av1 --cbr 300)."
                    .into(),
            );
        }
        // MKV stores raw 8-bit video as I420, not the card's NV12. This only reorders
        // the color planes, it doesn't change any pixel values.
        let conv = make("videoconvert")?;
        if conv.find_property("n-threads").is_some() {
            conv.set_property("n-threads", 0u32); // 0 = one thread per CPU core
        }
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", "I420")
                .build(),
        );
        return Ok(vec![conv, caps]);
    }
    if v.ten_bit && !v.codec.supports_10bit() {
        return Err("H.264 can't record 10-bit HDR. Use --codec hevc or --codec av1.".into());
    }

    let (hw, sw, parser) = match v.codec {
        Codec::H264 => ("vah264enc", "x264enc", "h264parse"),
        Codec::Hevc => ("vah265enc", "x265enc", "h265parse"),
        Codec::Av1 => ("vaav1enc", "svtav1enc", "av1parse"),
        Codec::Raw => unreachable!(),
    };

    let mut chain = Vec::new();
    match make(hw) {
        Ok(enc) => {
            log::debug(&format!("video encoder: {hw} (GPU)"));
            // vapostproc moves frames into GPU memory in the format the encoder wants.
            chain.push(make("vapostproc")?);
            configure_va(&enc, v);
            chain.push(enc);
        }
        Err(_) => {
            log::warn(&format!(
                "GPU encoder {hw} not available, using the software encoder {sw} (higher CPU load)"
            ));
            chain.push(make("videoconvert")?);
            // Pin 4:2:0 at the right bit depth: left alone, the converter can pick 8-bit
            // for AV1 or 4:4:4 for HEVC.
            let caps = make("capsfilter")?;
            caps.set_property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", if v.ten_bit { "I420_10LE" } else { "I420" })
                    .build(),
            );
            chain.push(caps);
            let enc = make(sw).map_err(|_| {
                format!(
                    "no {} encoder found: neither {hw} nor {sw} is installed",
                    v.codec.name()
                )
            })?;
            configure_software(&enc, v);
            chain.push(enc);
        }
    }
    chain.push(make(parser)?);
    Ok(chain)
}

fn configure_va(enc: &gst::Element, v: &VideoSettings) {
    set(enc, "key-int-max", &v.keyframe_interval.to_string());
    // No B-frames: frames stay in order, which keeps encoder latency low and lets the
    // MKV convert to MP4 with correct timing (MKV doesn't store decode timestamps).
    set(enc, "b-frames", "0");
    match v.rate {
        RateControl::Cbr { kbps } => {
            set(enc, "rate-control", "cbr");
            set(enc, "bitrate", &kbps.to_string());
        }
        RateControl::Vbr { kbps, max_kbps } => {
            // VA-API's VBR: bitrate is the ceiling, target-percentage the average.
            set(enc, "rate-control", "vbr");
            set(enc, "bitrate", &max_kbps.to_string());
            let pct = (kbps as u64 * 100 / max_kbps.max(1) as u64).clamp(1, 100);
            set(enc, "target-percentage", &pct.to_string());
        }
        RateControl::Quality { q } => {
            set(enc, "rate-control", "cqp");
            let q = q.to_string();
            // H.264/HEVC use qpi/qpp/qpb; the AV1 encoder uses a single qp.
            for prop in ["qpi", "qpp", "qpb", "qp"] {
                if enc.find_property(prop).is_some() {
                    set(enc, prop, &q);
                }
            }
        }
    }
}

fn configure_software(enc: &gst::Element, v: &VideoSettings) {
    let name = enc
        .factory()
        .map(|f| f.name().to_string())
        .unwrap_or_default();
    set(enc, "key-int-max", &v.keyframe_interval.to_string());
    match name.as_str() {
        "x264enc" | "x265enc" => {
            set(enc, "speed-preset", "veryfast");
            if name == "x264enc" {
                set(enc, "bframes", "0");
            } else {
                set(enc, "option-string", "bframes=0");
            }
            match v.rate {
                RateControl::Cbr { kbps } | RateControl::Vbr { kbps, .. } => {
                    set(enc, "bitrate", &kbps.to_string());
                }
                RateControl::Quality { q } => {
                    if name == "x264enc" {
                        set(enc, "pass", "quant");
                        set(enc, "quantizer", &q.to_string());
                    } else {
                        set(enc, "qp", &q.to_string());
                    }
                }
            }
        }
        "svtav1enc" => match v.rate {
            RateControl::Cbr { kbps } | RateControl::Vbr { kbps, .. } => {
                set(enc, "target-bitrate", &kbps.to_string());
            }
            RateControl::Quality { q } => {
                // SVT-AV1's scale runs 0 to 63; map our 0 to 51 onto it.
                set(enc, "cqp", &(q * 63 / 51).to_string());
            }
        },
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Aac,
    Opus,
    /// Uncompressed. Only fits in MKV.
    Pcm,
}

impl AudioCodec {
    pub fn parse(s: &str) -> Option<AudioCodec> {
        match s.to_ascii_lowercase().as_str() {
            "aac" => Some(AudioCodec::Aac),
            "opus" => Some(AudioCodec::Opus),
            "pcm" | "wav" | "lossless" => Some(AudioCodec::Pcm),
            _ => None,
        }
    }
}

/// Elements that encode one audio track (after convert/resample to 48 kHz stereo).
pub fn audio_chain(codec: AudioCodec, kbps: u32) -> Result<Vec<gst::Element>, String> {
    Ok(match codec {
        AudioCodec::Aac => {
            let enc = make("avenc_aac")
                .or_else(|_| make("fdkaacenc"))
                .or_else(|_| make("voaacenc"))
                .map_err(|_| "no AAC encoder found (install gst-libav)".to_string())?;
            set(&enc, "bitrate", &(kbps * 1000).to_string());
            vec![enc, make("aacparse")?]
        }
        AudioCodec::Opus => {
            let enc = make("opusenc")?;
            set(&enc, "bitrate", &(kbps * 1000).to_string());
            vec![enc, make("opusparse")?]
        }
        AudioCodec::Pcm => {
            let caps = make("capsfilter")?;
            caps.set_property(
                "caps",
                gst::Caps::builder("audio/x-raw")
                    .field("format", "S24LE")
                    .build(),
            );
            vec![make("audioconvert")?, caps]
        }
    })
}

pub fn make(factory: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(factory)
        .build()
        .map_err(|_| format!("GStreamer element `{factory}` is missing. Is its plugin installed?"))
}

/// Set a property from a string if the element has it. Encoders differ a lot between
/// versions, so a missing property is logged instead of failing the recording.
fn set(el: &gst::Element, prop: &str, value: &str) {
    if el.find_property(prop).is_some() {
        el.set_property_from_str(prop, value);
        log::debug(&format!("  {prop}={value}"));
    } else {
        log::debug(&format!(
            "  {} has no `{prop}` property, skipping",
            el.factory()
                .map(|f| f.name().to_string())
                .unwrap_or_default()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codecs() {
        assert_eq!(Codec::parse("HEVC"), Some(Codec::Hevc));
        assert_eq!(Codec::parse("h265"), Some(Codec::Hevc));
        assert_eq!(Codec::parse("vp9"), None);
        assert!(!Codec::H264.supports_10bit());
    }

    #[test]
    fn rejects_10bit_h264() {
        gst::init().unwrap();
        let v = VideoSettings {
            codec: Codec::H264,
            rate: RateControl::Cbr { kbps: 40_000 },
            keyframe_interval: 120,
            ten_bit: true,
        };
        assert!(video_chain(&v).is_err());
    }
}
