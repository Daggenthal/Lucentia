//! Recording: The card's video and audio, an optional mic, to a file.
//!
//!   video: card -> [HDR tag / MJPEG decode] -> tee -> big queue -> encoder -> mux
//!                                              tee -> preview window (optional)
//!   game audio: card audio -> 48 kHz stereo -> tee -> encoder -> mux  (track "Game")
//!   mic:        mic -> 48 kHz stereo -> sync offset -> tee -> encoder -> mux  ("Mic")
//!
//! The "Game + Mic" track is mixed after recording, from the finished Game and Mic
//! tracks (see remux.rs). Mixing two live devices in real time was choppy because each
//! runs on its own clock; offline there's nothing to drift.
//!
//! The file is always written as MKV first: if anything crashes mid-recording, an MKV
//! is still playable up to that point, where an unfinished MP4 is unreadable. When MP4
//! was asked for, it's converted on stop without re-encoding (see remux.rs).

use gst::prelude::*;
use gstreamer as gst;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::devices::{self, CaptureDevice, Kind, Mode};
use crate::encode::{self, AudioCodec, Codec, RateControl, VideoSettings};
use crate::estimate;
use crate::log;
use crate::preview::{self, make};
use crate::remux;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mkv,
}

pub struct Options {
    pub capture: preview::Options,
    pub codec: Option<Codec>,
    pub rate: Option<RateControl>,
    /// Seconds between keyframes.
    pub keyframe_secs: f64,
    /// Every frame a keyframe, for frame-accurate analysis.
    pub analysis: bool,
    pub audio_codec: AudioCodec,
    pub audio_kbps: u32,
    /// Microphone: a name to match, or "default".
    pub mic: Option<String>,
    /// Delay the mic by this many milliseconds (negative = earlier).
    pub mic_offset_ms: i64,
    /// Add a "Game + Mic" track (the first audio track, so players play it).
    pub mix_track: bool,
    /// Volume of the game and mic audio in percent (100 = unchanged).
    pub game_volume: f64,
    pub mic_volume: f64,
    pub container: Container,
    pub output: Option<PathBuf>,
    pub duration: Option<f64>,
    pub preview: bool,
    /// Record a test pattern and tones instead of a real card (for development).
    pub test_source: bool,
}

/// Default bitrate: 40 Mbit/s at 1080p60, scaled by pixels per second, within 10 to 200.
pub fn default_kbps(mode: &Mode) -> u32 {
    let ratio = (mode.width as f64 * mode.height as f64 * mode.fps()) / (1920.0 * 1080.0 * 60.0);
    (40_000.0 * ratio).clamp(10_000.0, 200_000.0).round() as u32
}

pub fn run(card: Option<&CaptureDevice>, opts: &Options) -> Result<(), String> {
    // ---- Work out the settings ------------------------------------------------------
    let mode = match card {
        Some(c) => preview::choose_mode(c, &opts.capture)?,
        None => test_mode(&opts.capture),
    };
    let hdr = preview::is_hdr(&mode, opts.capture.hdr_signal);
    let codec = opts
        .codec
        .unwrap_or(if hdr { Codec::Av1 } else { Codec::Hevc });
    if hdr && codec == Codec::H264 {
        return Err("H.264 can't record HDR. Use --codec hevc or --codec av1.".into());
    }
    if codec == Codec::Raw && mode.kind == Kind::Mjpeg {
        return Err(
            "raw recording needs a lossless mode; MJPEG modes are already compressed by the card"
                .into(),
        );
    }
    if codec == Codec::Hevc && mode.ten_bit() {
        log::warn(
            "10-bit HEVC files get labeled \"Main\" instead of \"Main 10\" by the GPU encoder \
             (a known GStreamer issue). They play fine in most players; use --codec av1 if a \
             tool rejects them.",
        );
    }
    if mode.kind == Kind::Mjpeg {
        log::warn("MJPEG modes are compressed by the card before we re-encode them, so expect some quality loss");
    }
    let mut container = opts.container;
    if opts.audio_codec == AudioCodec::Pcm && container == Container::Mp4 {
        log::warn("PCM audio doesn't fit in MP4, keeping the recording as MKV");
        container = Container::Mkv;
    }

    let rate = opts.rate.unwrap_or(RateControl::Cbr {
        kbps: default_kbps(&mode),
    });
    let keyframe_interval = if opts.analysis {
        1
    } else {
        (opts.keyframe_secs * mode.fps()).round().max(1.0) as u32
    };
    let video = VideoSettings {
        codec,
        rate,
        keyframe_interval,
        ten_bit: mode.ten_bit(),
    };

    let game_audio = opts.capture.audio;
    let audio_tracks = game_audio as u32
        + opts.mic.is_some() as u32
        + (opts.mix_track && opts.mic.is_some()) as u32;

    let final_path = opts.output.clone().unwrap_or_else(default_output);
    let final_path = with_extension(&final_path, container);
    let mkv_path = with_extension(&final_path, Container::Mkv);
    if let Some(dir) = final_path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("can't create {}: {e}", dir.display()))?;
    }

    // ---- Tell the user what's about to happen ---------------------------------------
    let est = estimate::rate(&estimate::Inputs {
        codec,
        rate,
        width: mode.width,
        height: mode.height,
        fps: mode.fps(),
        ten_bit: mode.ten_bit(),
        audio_codec: opts.audio_codec,
        audio_kbps: opts.audio_kbps,
        audio_tracks,
    });
    print_plan(
        card,
        &mode,
        hdr,
        &video,
        opts,
        audio_tracks,
        container,
        &final_path,
        est,
    );
    check_disk(&final_path, est, codec);

    // ---- Build the pipeline ---------------------------------------------------------
    let pipeline = gst::Pipeline::new();
    let mux = make("matroskamux")?;
    if mux.find_property("writing-app").is_some() {
        mux.set_property("writing-app", "Lucentia");
    }
    let sink = make("filesink")?;
    sink.set_property("location", mkv_path.to_string_lossy().to_string());
    pipeline
        .add_many([&mux, &sink])
        .map_err(|e| e.to_string())?;
    mux.link(&sink)
        .map_err(|_| "could not connect the muxer to the file")?;

    // Video.
    let (mut cap_chain, probe_point) = match card {
        Some(c) => preview::capture_chain(c, &mode, hdr)?,
        None => test_video(&mode, hdr)?,
    };
    let vtee = make("tee")?;
    cap_chain.push(vtee.clone());
    add_chain(&pipeline, &cap_chain, "video capture")?;

    // A big queue before the encoder so frames wait in memory while it starts up,
    // instead of being dropped (the 50 lost frames in our first manual test).
    let mut enc_chain = vec![unbounded_queue()?];
    enc_chain.extend(encode::video_chain(&video)?);
    add_chain(&pipeline, &enc_chain, "video encoder")?;
    vtee.link(&enc_chain[0])
        .map_err(|_| "could not connect the video to the encoder")?;
    link_to_mux(enc_chain.last().unwrap(), &mux, "Video")?;

    if opts.preview {
        let q = make("queue")?;
        q.set_property_from_str("leaky", "downstream");
        q.set_property("max-size-buffers", 2u32);
        let mut disp = vec![q];
        disp.extend(preview::display_chain(hdr)?);
        add_chain(&pipeline, &disp, "preview")?;
        vtee.link(&disp[0])
            .map_err(|_| "could not connect the preview")?;
    }

    // Audio.

    if game_audio {
        let src = match card {
            Some(c) => match devices::find_audio_for(c) {
                Some(dev) => {
                    println!(
                        "{} game audio from {}",
                        log::green("found"),
                        dev.display_name()
                    );
                    Some(preview::audio_source(&dev)?)
                }
                None => {
                    log::warn("couldn't find this card's audio, recording without game audio");
                    None
                }
            },
            None => Some(test_tone(440.0)?),
        };
        if let Some(src) = src {
            add_audio_track(&pipeline, src, &mux, "Game", 0, opts.game_volume, opts)?;
        }
    }

    if let Some(name) = &opts.mic {
        let src = if opts.test_source {
            test_tone(880.0)?
        } else {
            let dev = devices::find_mic(name).ok_or_else(|| {
                format!("no microphone matches `{name}`. Run `lucentia mics` to list them.")
            })?;
            println!("{} mic {}", log::green("found"), dev.display_name());
            preview::audio_source(&dev)?
        };
        add_audio_track(
            &pipeline,
            src,
            &mux,
            "Mic",
            opts.mic_offset_ms,
            opts.mic_volume,
            opts,
        )?;
    }

    // ---- Watch frames for gaps (lost frames) ----------------------------------------
    let stats = Arc::new(FrameStats::new(mode.fps()));
    {
        let stats = stats.clone();
        probe_point
            .static_pad("src")
            .ok_or("capsfilter has no src pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buf) = info.buffer() {
                    stats.frame(buf.pts());
                }
                gst::PadProbeReturn::Ok
            });
    }

    // ---- Run ------------------------------------------------------------------------
    let stopping = Arc::new(AtomicBool::new(false));
    {
        let weak = pipeline.downgrade();
        let stopping = stopping.clone();
        let _ = ctrlc::set_handler(move || {
            if stopping.swap(true, Ordering::SeqCst) {
                // Second Ctrl+C: give up on finishing the file cleanly.
                eprintln!(
                    "{}",
                    log::red("stopping immediately; the MKV is still playable")
                );
                std::process::exit(130);
            }
            println!(
                "\n{} finishing the file (press Ctrl+C again to force quit)",
                log::yellow("stopping:")
            );
            if let Some(p) = weak.upgrade() {
                p.send_event(gst::event::Eos::new());
            }
        });
    }

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| "recording failed to start. Run with --debug for details.".to_string())?;
    println!(
        "{} {}  (Ctrl+C to stop)",
        log::green("recording"),
        mkv_path.display()
    );

    let result = watch(&pipeline, &stats, &mkv_path, opts.duration, &stopping);
    let _ = pipeline.set_state(gst::State::Null);
    result?;

    // ---- Finish ---------------------------------------------------------------------
    let size = std::fs::metadata(&mkv_path).map(|m| m.len()).unwrap_or(0);
    let lost = stats.lost.load(Ordering::Relaxed);
    if lost > 0 {
        log::warn(&format!("{lost} frame(s) were lost during the recording"));
    } else {
        log::ok("no frames lost");
    }

    let add_mix = opts.mix_track && opts.mic.is_some() && game_audio;
    if container == Container::Mp4 || add_mix {
        let what = match (add_mix, container) {
            (true, Container::Mp4) => "mixing Game + Mic and converting to MP4",
            (true, Container::Mkv) => "mixing Game + Mic",
            _ => "converting to MP4",
        };
        println!(
            "{} {what} (video is copied, not re-encoded)",
            log::dim("finishing:")
        );
        let mix = add_mix.then_some((opts.audio_codec, opts.audio_kbps));
        match remux::finalize(&mkv_path, &final_path, container == Container::Mp4, mix) {
            Ok(()) => {
                let out_size = std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);
                log::ok(&format!(
                    "saved {} ({})",
                    final_path.display(),
                    estimate::human_bytes(out_size as f64)
                ));
            }
            Err(e) => {
                log::warn(&format!(
                    "finishing failed ({e}); your recording is safe as MKV with separate tracks"
                ));
                log::ok(&format!(
                    "saved {} ({})",
                    mkv_path.display(),
                    estimate::human_bytes(size as f64)
                ));
            }
        }
    } else {
        log::ok(&format!(
            "saved {} ({})",
            mkv_path.display(),
            estimate::human_bytes(size as f64)
        ));
    }
    Ok(())
}

/// Audio source -> 48 kHz stereo -> [volume] -> tee -> encoder -> mux, plus a branch
/// to the speakers for the game audio while previewing. A nonzero offset shifts the
/// whole track.
fn add_audio_track(
    pipeline: &gst::Pipeline,
    src: gst::Element,
    mux: &gst::Element,
    title: &str,
    offset_ms: i64,
    volume_pct: f64,
    opts: &Options,
) -> Result<(), String> {
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 48_000i32)
            .field("channels", 2i32)
            .build(),
    );
    let mut chain = vec![
        src,
        make("queue")?,
        make("audioconvert")?,
        make("audioresample")?,
        caps,
    ];
    if (volume_pct - 100.0).abs() > 0.01 {
        let vol = make("volume")?;
        vol.set_property("volume", volume_pct / 100.0);
        chain.push(vol);
    }
    let last = chain.last().unwrap().clone();
    let tee = make("tee")?;
    chain.push(tee.clone());
    add_chain(pipeline, &chain, title)?;
    if offset_ms != 0 {
        // Shift this track's timestamps: positive delays it, negative moves it earlier.
        // Lines the mic up with video that reaches us a little late through the card.
        let pad = last.static_pad("src").ok_or("audio chain has no src pad")?;
        pad.set_offset(offset_ms * 1_000_000);
    }

    let mut enc = vec![make("queue")?];
    enc.extend(encode::audio_chain(opts.audio_codec, opts.audio_kbps)?);
    add_chain(pipeline, &enc, title)?;
    tee.link(&enc[0])
        .map_err(|_| format!("could not connect the {title} track"))?;
    link_to_mux(enc.last().unwrap(), mux, title)?;

    if opts.preview && title == "Game" {
        // Play the game audio while the preview window is open.
        let q = make("queue")?;
        q.set_property_from_str("leaky", "downstream");
        let listen = [q, make("audioconvert")?, make("autoaudiosink")?];
        add_chain(pipeline, &listen, "preview audio")?;
        tee.link(&listen[0])
            .map_err(|_| "could not connect the preview audio")?;
    }

    Ok(())
}

/// Request a pad on the muxer, link to it, and give the track a name players show.
fn link_to_mux(from: &gst::Element, mux: &gst::Element, title: &str) -> Result<(), String> {
    let src = from.static_pad("src").ok_or("element has no src pad")?;
    let caps = src.query_caps(None);
    let template = if caps
        .structure(0)
        .map(|s| s.name().starts_with("audio/"))
        .unwrap_or(false)
    {
        "audio_%u"
    } else {
        "video_%u"
    };
    let sink = mux
        .request_pad_simple(template)
        .ok_or_else(|| format!("the muxer refused the {title} track"))?;
    // Name the track ("Game", "Mic") right before the muxer, so tags sent by the audio
    // source can't override it. Players and editors show these names (MKV only).
    let bin = from
        .parent()
        .and_then(|p| p.downcast::<gst::Bin>().ok())
        .ok_or("element is not in a pipeline")?;
    let name_it = make("taginject")?;
    name_it.set_property("tags", format!("title=\"{title}\""));
    bin.add(&name_it).map_err(|e| e.to_string())?;
    from.link(&name_it)
        .map_err(|_| format!("could not name the {title} track"))?;
    name_it
        .static_pad("src")
        .unwrap()
        .link(&sink)
        .map_err(|e| format!("could not connect the {title} track to the muxer: {e:?}"))?;
    Ok(())
}

fn add_chain(pipeline: &gst::Pipeline, chain: &[gst::Element], what: &str) -> Result<(), String> {
    pipeline
        .add_many(chain)
        .map_err(|e| format!("could not build the {what} pipeline: {e}"))?;
    if chain.len() > 1 {
        gst::Element::link_many(chain).map_err(|_| {
            format!("could not connect the {what} pipeline (run with --debug for details)")
        })?;
    }
    Ok(())
}

fn unbounded_queue() -> Result<gst::Element, String> {
    let q = make("queue")?;
    q.set_property("max-size-buffers", 0u32);
    q.set_property("max-size-bytes", 0u32);
    q.set_property("max-size-time", 0u64);
    Ok(q)
}

// ---- Test source (development only) -------------------------------------------------

fn test_mode(o: &preview::Options) -> Mode {
    let (w, h, fps) = o.mode.unwrap_or((1280, 720, 60.0));
    Mode {
        kind: Kind::Lossless,
        width: w,
        height: h,
        fps_num: (fps * 1000.0).round() as i32,
        fps_den: 1000,
        format: if o.hdr { "P010_10LE" } else { "NV12" }.into(),
    }
}

fn test_video(mode: &Mode, hdr: bool) -> Result<(Vec<gst::Element>, gst::Element), String> {
    let src = make("videotestsrc")?;
    src.set_property("is-live", true);
    src.set_property_from_str("pattern", "ball");
    let caps = make("capsfilter")?;
    caps.set_property("caps", preview::mode_caps(mode));
    let mut chain = vec![src, caps.clone()];
    if hdr {
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

fn test_tone(freq: f64) -> Result<gst::Element, String> {
    let src = make("audiotestsrc")?;
    src.set_property("is-live", true);
    src.set_property("freq", freq);
    src.set_property("volume", 0.2f64);
    Ok(src)
}

// ---- Stats --------------------------------------------------------------------------

struct FrameStats {
    frames: AtomicU64,
    lost: AtomicU64,
    last_pts: Mutex<Option<gst::ClockTime>>,
    frame_ns: f64,
}

impl FrameStats {
    fn new(fps: f64) -> Self {
        FrameStats {
            frames: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            last_pts: Mutex::new(None),
            frame_ns: 1e9 / fps.max(1.0),
        }
    }

    /// Count a frame, and any frames missing between it and the previous one.
    fn frame(&self, pts: Option<gst::ClockTime>) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        let Some(pts) = pts else { return };
        let mut last = self.last_pts.lock().unwrap();
        if let Some(prev) = *last {
            let gap = pts.nseconds() as f64 - prev.nseconds() as f64;
            if gap > self.frame_ns * 1.5 {
                let missing = (gap / self.frame_ns).round() as u64 - 1;
                self.lost.fetch_add(missing, Ordering::Relaxed);
                log::debug(&format!(
                    "gap of {:.1} ms: {missing} frame(s) lost",
                    gap / 1e6
                ));
            }
        }
        *last = Some(pts);
    }
}

fn watch(
    pipeline: &gst::Pipeline,
    stats: &FrameStats,
    file: &Path,
    duration: Option<f64>,
    stopping: &AtomicBool,
) -> Result<(), String> {
    let bus = pipeline.bus().ok_or("pipeline has no bus")?;
    let start = Instant::now();
    let mut last = (Instant::now(), 0u64);
    let mut warned_lost = 0u64;
    loop {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(250)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => return Ok(()),
                MessageView::Error(e) => {
                    let text = e.error().to_string();
                    if text.contains("window was closed") {
                        // Closing the preview stops the recording cleanly.
                        if !stopping.swap(true, Ordering::SeqCst) {
                            println!("{}", log::yellow("preview closed, finishing the recording"));
                            pipeline.send_event(gst::event::Eos::new());
                        }
                        continue;
                    }
                    let debug = e.debug().map(|d| d.to_string()).unwrap_or_default();
                    log::debug(&format!(
                        "from {:?}: {debug}",
                        e.src().map(|s| s.path_string())
                    ));
                    return Err(format!(
                        "{text} (the MKV recorded so far is still playable)"
                    ));
                }
                MessageView::Warning(w) => log::warn(&w.error().to_string()),
                _ => {}
            }
        }

        if let Some(d) = duration {
            if start.elapsed().as_secs_f64() >= d && !stopping.swap(true, Ordering::SeqCst) {
                println!(
                    "{} reached --duration, finishing the file",
                    log::dim("stopping:")
                );
                pipeline.send_event(gst::event::Eos::new());
            }
        }

        if last.0.elapsed() >= Duration::from_secs(5) && !stopping.load(Ordering::Relaxed) {
            let frames = stats.frames.load(Ordering::Relaxed);
            let fps = (frames - last.1) as f64 / last.0.elapsed().as_secs_f64();
            let elapsed = start.elapsed().as_secs_f64();
            let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0) as f64;
            let bytes_per_sec = size / elapsed.max(1.0);
            let left = estimate::free_space(file)
                .map(|free| {
                    format!(
                        ", {} left on drive",
                        estimate::human_duration(free as f64 / bytes_per_sec.max(1.0))
                    )
                })
                .unwrap_or_default();
            println!(
                "  {}  {}  {:.1} Mbit/s  {fps:.2} fps{left}",
                estimate::human_duration(elapsed),
                estimate::human_bytes(size),
                bytes_per_sec * 8.0 / 1e6,
            );
            let lost = stats.lost.load(Ordering::Relaxed);
            if lost > warned_lost {
                log::warn(&format!("{lost} frame(s) lost so far"));
                warned_lost = lost;
            }
            last = (Instant::now(), frames);
        }
    }
}

// ---- Output and messages ------------------------------------------------------------

fn with_extension(p: &Path, c: Container) -> PathBuf {
    p.with_extension(match c {
        Container::Mp4 => "mp4",
        Container::Mkv => "mkv",
    })
}

/// ~/Videos/Lucentia/Lucentia 2026-11-04 18-30-05.mkv
fn default_output() -> PathBuf {
    let base = std::env::var_os("XDG_VIDEOS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Lucentia")
        .join(format!("Lucentia {}.mkv", local_timestamp()))
}

fn local_timestamp() -> String {
    // SAFETY: time() and localtime_r() only write into the struct we pass.
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn print_plan(
    card: Option<&CaptureDevice>,
    mode: &Mode,
    hdr: bool,
    v: &VideoSettings,
    opts: &Options,
    audio_tracks: u32,
    container: Container,
    path: &Path,
    est: estimate::Rate,
) {
    let source = card.map(|c| c.name.as_str()).unwrap_or("test pattern");
    let depth = if mode.ten_bit() { "10-bit" } else { "8-bit" };
    println!(
        "{} {source} at {}x{}@{} {depth}{}",
        log::bold("source:"),
        mode.width,
        mode.height,
        preview::fmt_fps(mode.fps()),
        if hdr { " HDR" } else { "" }
    );
    let rate = match (v.codec, v.rate) {
        (Codec::Raw, _) => "uncompressed".to_string(),
        (_, RateControl::Cbr { kbps }) => format!("CBR {} Mbit/s", mbit(kbps)),
        (_, RateControl::Vbr { kbps, max_kbps }) => {
            format!("VBR {} Mbit/s, max {}", mbit(kbps), mbit(max_kbps))
        }
        (_, RateControl::Quality { q }) => format!("constant quality {q}"),
    };
    let kf = if v.codec == Codec::Raw {
        "every frame stands alone".to_string()
    } else if v.keyframe_interval == 1 {
        "every frame a keyframe (analysis)".to_string()
    } else {
        format!("keyframe every {} frames", v.keyframe_interval)
    };
    println!("{} {}, {rate}, {kf}", log::bold("video:"), v.codec.name());
    let audio = if audio_tracks == 0 {
        "none".to_string()
    } else {
        let mut t = Vec::new();
        if opts.mic.is_some() && opts.mix_track {
            t.push("Game + Mic");
        }
        if opts.capture.audio {
            t.push("Game");
        }
        if opts.mic.is_some() {
            t.push("Mic");
        }
        let codec = match opts.audio_codec {
            AudioCodec::Aac => format!("AAC {} kbit/s", opts.audio_kbps),
            AudioCodec::Opus => format!("Opus {} kbit/s", opts.audio_kbps),
            AudioCodec::Pcm => "PCM (lossless)".into(),
        };
        format!("{} ({codec})", t.join(", "))
    };
    println!("{} {audio}", log::bold("audio:"));
    if opts.mic.is_some() && opts.mic_offset_ms != 0 {
        println!(
            "{} mic shifted {} ms",
            log::bold("sync:"),
            opts.mic_offset_ms
        );
    }
    println!(
        "{} {} ({})",
        log::bold("file:"),
        path.display(),
        if container == Container::Mp4 {
            "MP4: recorded as MKV for crash safety, converted when you stop"
        } else {
            "MKV: still playable if anything crashes mid-recording"
        }
    );
    println!("{} {}", log::bold("size:"), estimate::describe(est));
}

/// 17778 kbit/s -> "17.8", 40000 -> "40".
fn mbit(kbps: u32) -> String {
    let m = kbps as f64 / 1000.0;
    if (m - m.round()).abs() < 0.05 {
        format!("{}", m.round() as u64)
    } else {
        format!("{m:.1}")
    }
}

fn check_disk(path: &Path, est: estimate::Rate, codec: Codec) {
    let Some(free) = estimate::free_space(path) else {
        return;
    };
    let secs = free as f64 / est.high.max(1.0);
    let line = format!(
        "{} free, enough for about {} at this rate",
        estimate::human_bytes(free as f64),
        estimate::human_duration(secs)
    );
    if secs < 3600.0 {
        log::warn(&line);
    } else {
        println!("{} {line}", log::bold("drive:"));
    }
    if codec == Codec::Raw {
        log::warn(&format!(
            "raw recording needs about {}/s of sustained disk writes; slower drives will lose frames",
            estimate::human_bytes(est.high)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bitrate_scales_with_mode() {
        let m = |w, h, fps: i32| Mode {
            kind: Kind::Lossless,
            width: w,
            height: h,
            fps_num: fps,
            fps_den: 1,
            format: "NV12".into(),
        };
        assert_eq!(default_kbps(&m(1920, 1080, 60)), 40_000);
        assert_eq!(default_kbps(&m(3840, 2160, 60)), 160_000);
        assert_eq!(default_kbps(&m(1280, 720, 30)), 10_000);
    }

    #[test]
    fn counts_lost_frames_from_gaps() {
        let s = FrameStats::new(60.0);
        // 60 fps = 16.67 ms per frame. 0, 16.7, then 66.7 (two missing), then 83.3.
        s.frame(Some(gst::ClockTime::from_nseconds(0)));
        s.frame(Some(gst::ClockTime::from_nseconds(16_666_667)));
        s.frame(Some(gst::ClockTime::from_nseconds(66_666_667)));
        s.frame(Some(gst::ClockTime::from_nseconds(83_333_333)));
        assert_eq!(s.lost.load(Ordering::Relaxed), 2);
        assert_eq!(s.frames.load(Ordering::Relaxed), 4);
    }
}
