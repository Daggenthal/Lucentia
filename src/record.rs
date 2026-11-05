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
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
    /// Record at this frame rate instead of the capture's (e.g. 40 from 120 Hz,
    /// keeping every third frame). None = the capture's rate.
    pub record_fps: Option<u32>,
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
    /// Write a file. Off = preview only (the app window before Record is pressed).
    pub record: bool,
    /// Send preview frames here (the app window) instead of opening a preview window.
    pub frames: Option<preview::FrameSink>,
    /// Start with the game audio muted on the speakers (recording is unaffected).
    pub listen_muted: bool,
    /// Start with the mic muted in the recording.
    pub mic_muted: bool,
    /// Remove the game's sound from the mic (when speakers play it into the mic).
    pub echo_cancel: bool,
}

/// Live controls for a running session: change volume and mute without restarting.
#[derive(Clone, Default)]
pub struct Controls {
    game_volume: Option<gst::Element>,
    mic_volume: Option<gst::Element>,
    listen: Option<gst::Element>,
}

impl Controls {
    /// Game volume in percent, for both the recording and what you hear.
    pub fn set_game_volume(&self, pct: f64) {
        if let Some(v) = &self.game_volume {
            v.set_property("volume", pct / 100.0);
        }
    }
    pub fn set_mic_volume(&self, pct: f64) {
        if let Some(v) = &self.mic_volume {
            v.set_property("volume", pct / 100.0);
        }
    }
    /// Mute what plays on the speakers. The recording keeps the game audio.
    pub fn set_listen_muted(&self, muted: bool) {
        if let Some(v) = &self.listen {
            v.set_property("mute", muted);
        }
    }
    /// Mute the mic in the recording.
    pub fn set_mic_muted(&self, muted: bool) {
        if let Some(v) = &self.mic_volume {
            v.set_property("mute", muted);
        }
    }
}

/// Default bitrate: 40 Mbit/s for H.264 at 1080p60, scaled by pixels per second and
/// by how efficient the codec is (HEVC and AV1 look the same with fewer bits), within
/// 10 to 200 Mbit/s.
pub fn default_kbps(mode: &Mode, codec: Codec) -> u32 {
    let ratio = (mode.width as f64 * mode.height as f64 * mode.fps()) / (1920.0 * 1080.0 * 60.0);
    let efficiency = match codec {
        Codec::Hevc => 0.7,
        Codec::Av1 => 0.6,
        _ => 1.0,
    };
    (40_000.0 * ratio * efficiency)
        .clamp(10_000.0, 200_000.0)
        .round() as u32
}

/// Everything decided before a session starts: the mode, encoder settings, where the
/// file goes and how big it will get. Warnings are collected for the caller to show.
#[derive(Clone)]
pub struct Plan {
    pub mode: Mode,
    pub hdr: bool,
    pub video: VideoSettings,
    pub container: Container,
    pub final_path: PathBuf,
    pub mkv_path: PathBuf,
    pub est: estimate::Rate,
    pub game_audio: bool,
    pub audio_tracks: u32,
    pub warnings: Vec<String>,
}

pub fn plan(card: Option<&CaptureDevice>, opts: &Options) -> Result<Plan, String> {
    let mode = match card {
        Some(c) => preview::choose_mode(c, &opts.capture)?,
        None => test_mode(&opts.capture),
    };
    let hdr = preview::is_hdr(&mode, opts.capture.hdr_signal);
    let codec = opts
        .codec
        .unwrap_or(if hdr { Codec::Av1 } else { Codec::Hevc });
    if hdr && codec == Codec::H264 {
        return Err("H.264 can't record HDR. Use HEVC or AV1.".into());
    }
    if codec == Codec::Raw && mode.kind == Kind::Mjpeg {
        return Err(
            "raw recording needs a lossless mode; MJPEG modes are already compressed by the card"
                .into(),
        );
    }
    let mut warnings = Vec::new();
    if opts.record && codec == Codec::Hevc && mode.ten_bit() {
        warnings.push(
            "10-bit HEVC files get labeled \"Main\" instead of \"Main 10\" by the GPU encoder \
             (a known GStreamer issue). They play fine in most players; use AV1 if a tool \
             rejects them."
                .to_string(),
        );
    }
    if opts.record && mode.kind == Kind::Mjpeg {
        warnings.push(
            "MJPEG modes are compressed by the card before we re-encode them, so expect some quality loss"
                .to_string(),
        );
    }
    let mut container = opts.container;
    if opts.audio_codec == AudioCodec::Pcm && container == Container::Mp4 {
        warnings.push("PCM audio doesn't fit in MP4, keeping the recording as MKV".to_string());
        container = Container::Mkv;
    }

    let rate = opts.rate.unwrap_or(RateControl::Cbr {
        kbps: default_kbps(&mode, codec),
    });
    let output_fps = opts
        .record_fps
        .filter(|&f| (f as f64) < mode.fps() - 0.01);
    if let Some(f) = output_fps {
        let step = mode.fps() / f as f64;
        if (step - step.round()).abs() > 0.01 {
            warnings.push(format!(
                "{f} fps doesn't divide evenly into {} fps, so frame pacing will be uneven",
                preview::fmt_fps(mode.fps())
            ));
        }
    }
    let out_fps = output_fps.map(|f| f as f64).unwrap_or(mode.fps());
    let keyframe_interval = if opts.analysis {
        1
    } else {
        (opts.keyframe_secs * out_fps).round().max(1.0) as u32
    };
    let video = VideoSettings {
        codec,
        rate,
        keyframe_interval,
        ten_bit: mode.ten_bit(),
        output_fps,
    };

    let game_audio = opts.capture.audio;
    let audio_tracks = game_audio as u32
        + opts.mic.is_some() as u32
        + (opts.mix_track && opts.mic.is_some() && game_audio) as u32;

    let final_path = opts.output.clone().unwrap_or_else(default_output);
    let final_path = with_extension(&final_path, container);
    let mkv_path = with_extension(&final_path, Container::Mkv);

    let est = estimate::rate(&estimate::Inputs {
        codec,
        rate,
        width: mode.width,
        height: mode.height,
        fps: out_fps,
        ten_bit: mode.ten_bit(),
        audio_codec: opts.audio_codec,
        audio_kbps: opts.audio_kbps,
        audio_tracks,
    });
    Ok(Plan {
        mode,
        hdr,
        video,
        container,
        final_path,
        mkv_path,
        est,
        game_audio,
        audio_tracks,
        warnings,
    })
}

/// Live numbers for a running session.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    /// Seconds since the session started, or since the recording started while
    /// recording.
    pub elapsed: f64,
    /// Bytes written so far (0 when not recording).
    pub size: u64,
    pub mbps: f64,
    pub fps: f64,
    /// Frames lost; counted from the start of the recording while recording.
    pub lost: u64,
    /// Seconds of recording left on the drive at the current rate.
    pub drive_left: Option<f64>,
    pub recording: bool,
    /// Diagnostics (Settings > Debug), counted since the session started.
    pub damaged: u64,
    pub gaps: u64,
    pub worst_gap_ms: f64,
    pub lost_total: u64,
    /// Seconds of video waiting for the encoder (0 when it keeps up).
    pub encoder_backlog: f64,
    /// The video encoder in use while recording, e.g. "vah265enc".
    pub encoder: Option<String>,
    /// Lucentia's CPU use, as a share of the whole CPU (0 to 100).
    pub cpu_pct: f64,
}

/// The result of a finished recording.
pub struct Finished {
    pub path: PathBuf,
    pub size: u64,
    pub lost: u64,
    /// Set when the mix or MP4 step failed and the MKV was kept instead.
    pub finish_error: Option<String>,
}

/// Stops a session from another thread (Ctrl+C, closing the window).
#[derive(Clone)]
pub struct StopHandle {
    pipeline: gst::glib::WeakRef<gst::Pipeline>,
    stopping: Arc<AtomicBool>,
}

impl StopHandle {
    /// End the stream so every file gets closed properly.
    /// Returns false if a stop was already in progress.
    pub fn stop(&self) -> bool {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return false;
        }
        if let Some(p) = self.pipeline.upgrade() {
            p.send_event(gst::event::Eos::new());
        }
        true
    }
}

/// A recording attached to a running session.
struct ActiveRecording {
    bin: gst::Bin,
    /// (tee, tee pad, recording input) for each stream feeding the recording.
    links: Vec<(gst::Element, gst::Pad, gst::Pad)>,
    plan: Plan,
    started: Instant,
    lost_at_start: u64,
    /// The queue in front of the encoder: how far behind the encoder is.
    video_queue: gst::Element,
    encoder: String,
}

/// A recording that has been detached, waiting to be finished.
pub struct StoppedRecording {
    bin: gst::Bin,
    links: Vec<(gst::Element, gst::Pad, gst::Pad)>,
    pub plan: Plan,
    pub lost: u64,
    /// The recording was stopped by an error rather than by the user, so its last
    /// moments may be missing (the MKV is still playable).
    pub interrupted: bool,
}

/// A running capture: the card is opened once and stays open. Preview and the
/// speakers run the whole time; recordings attach to it and detach again without the
/// card (or the console on the other end of its HDMI) ever noticing.
///
///   card -> [drop damaged frames] -> tee --> preview
///                                        \-> recording (attached on Record)
///   card audio -> volume -> tee --> speakers
///                              \-> recording
///   mic -> volume -> tee -> recording
pub struct Session {
    pipeline: gst::Pipeline,
    stats: Arc<FrameStats>,
    pub controls: Controls,
    stopping: Arc<AtomicBool>,
    pub plan: Plan,
    video_tee: gst::Element,
    game_tee: Option<gst::Element>,
    mic_tee: Option<gst::Element>,
    mic_offset_ms: i64,
    started: Instant,
    recording: Mutex<Option<ActiveRecording>>,
    /// Recordings whose files are complete (their end of stream reached the file).
    finished_bins: Arc<(Mutex<Vec<String>>, std::sync::Condvar)>,
    next_recording: AtomicU64,
    /// The virtual camera output, when on: its bin and the tee pad feeding it.
    virtual_camera: Mutex<Option<(gst::Bin, gst::Pad)>>,
    /// Latest audio peaks in dB (f32 bits), game then mic, for the level meters.
    levels: Arc<[AtomicU32; 2]>,
}

/// Where the clean video feed goes, for OBS, Discord and browsers.
#[derive(Clone, Debug, PartialEq)]
pub enum VirtualCamera {
    /// A PipeWire camera node ("Lucentia"), no kernel module needed.
    PipeWire,
    /// A v4l2loopback device such as /dev/video10, which every camera app can open.
    Loopback(PathBuf),
}

/// What the level meters show when there's no sound (or no source).
const SILENT_DB: f32 = -100.0;

fn new_tee() -> Result<gst::Element, String> {
    let t = make("tee")?;
    // A tee with nothing attached yet (no recording) must not stop the stream.
    t.set_property("allow-not-linked", true);
    Ok(t)
}

/// Open the card and start the preview and audio. Recording is attached separately
/// with `Session::start_recording`.
pub fn start(card: Option<&CaptureDevice>, opts: &Options, plan: Plan) -> Result<Session, String> {
    let mode = plan.mode.clone();
    let hdr = plan.hdr;
    let pipeline = gst::Pipeline::new();

    // Video.
    let (mut cap_chain, probe_point) = match card {
        Some(c) => preview::capture_chain(c, &mode, hdr)?,
        None => test_video(&mode, hdr)?,
    };
    let video_tee = new_tee()?;
    cap_chain.push(video_tee.clone());
    add_chain(&pipeline, &cap_chain, "video capture")?;

    let showing = opts.frames.is_some() || opts.preview;
    if showing {
        let q = make("queue")?;
        q.set_property_from_str("leaky", "downstream");
        q.set_property("max-size-buffers", 2u32);
        let mut disp = vec![q];
        match &opts.frames {
            Some(sink) => {
                let (w, h) = preview::preview_size(&mode);
                disp.extend(preview::frames_chain(hdr, w, h, sink.clone())?);
            }
            None => disp.extend(preview::display_chain(hdr)?),
        }
        add_chain(&pipeline, &disp, "preview")?;
        video_tee
            .link(&disp[0])
            .map_err(|_| "could not connect the preview")?;
    }

    // Audio.
    let mut controls = Controls::default();
    let mut game_tee = None;
    if plan.game_audio {
        let src = match card {
            Some(c) => match devices::find_audio_for(c) {
                Some(dev) => {
                    log::debug(&format!("game audio from {}", dev.display_name()));
                    Some(preview::audio_source(&dev)?)
                }
                None => {
                    log::warn("couldn't find this card's audio, continuing without game audio");
                    None
                }
            },
            None => Some(test_tone(440.0)?),
        };
        if let Some(src) = src {
            let t = add_audio_source(&pipeline, src, "game audio", opts.game_volume, showing, None)?;
            if let Some(l) = &t.listen {
                l.set_property("mute", opts.listen_muted);
            }
            controls.game_volume = Some(t.volume);
            controls.listen = t.listen;
            game_tee = Some(t.tee);
        }
    }

    let mut mic_tee = None;
    if let Some(name) = &opts.mic {
        let src = if opts.test_source {
            test_tone(880.0)?
        } else {
            let dev = devices::find_mic(name).ok_or_else(|| {
                format!("no microphone matches `{name}`. Run `lucentia mics` to list them.")
            })?;
            log::debug(&format!("mic {}", dev.display_name()));
            preview::audio_source(&dev)?
        };
        // Echo cancellation: the game audio is the reference for what the speakers
        // play, and whatever of it the mic picks up is subtracted from the mic.
        let dsp = match (&game_tee, opts.echo_cancel) {
            (Some(game), true) => match echo_canceller(&pipeline, game) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::warn(&format!("echo cancellation unavailable: {e}"));
                    None
                }
            },
            _ => None,
        };
        let t = add_audio_source(&pipeline, src, "mic", opts.mic_volume, false, dsp)?;
        t.volume.set_property("mute", opts.mic_muted);
        controls.mic_volume = Some(t.volume);
        mic_tee = Some(t.tee);
    }

    // Drop damaged frames before anything sees them. The card's driver flags frames
    // it couldn't receive completely (the first frame after the capture starts, and
    // whenever the HDMI signal changes); shown, they're the green flashes. The first
    // two frames are dropped too, as the card is still settling.
    let stats = Arc::new(FrameStats::new(mode.fps()));
    {
        let seen = AtomicU64::new(0);
        let stats = stats.clone();
        probe_point
            .static_pad("src")
            .ok_or("capsfilter has no src pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let n = seen.fetch_add(1, Ordering::Relaxed);
                let damaged = info
                    .buffer()
                    .is_some_and(|b| b.flags().contains(gst::BufferFlags::CORRUPTED));
                if n < 2 || damaged {
                    if damaged {
                        stats.damaged.fetch_add(1, Ordering::Relaxed);
                        log::debug("dropped a damaged frame from the card");
                    }
                    gst::PadProbeReturn::Drop
                } else {
                    gst::PadProbeReturn::Ok
                }
            });
    }

    // Watch frames for gaps (lost frames).
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

    pipeline.set_state(gst::State::Playing).map_err(|_| {
        "the capture failed to start. Is another program using the card? Run with --debug for details."
            .to_string()
    })?;
    Ok(Session {
        pipeline,
        stats,
        controls,
        stopping: Arc::new(AtomicBool::new(false)),
        plan,
        video_tee,
        game_tee,
        mic_tee,
        mic_offset_ms: opts.mic_offset_ms,
        started: Instant::now(),
        recording: Mutex::new(None),
        finished_bins: Arc::new((Mutex::new(Vec::new()), std::sync::Condvar::new())),
        next_recording: AtomicU64::new(1),
        virtual_camera: Mutex::new(None),
        levels: Arc::new([
            AtomicU32::new(SILENT_DB.to_bits()),
            AtomicU32::new(SILENT_DB.to_bits()),
        ]),
    })
}

/// Loads v4l2loopback the way Chromium-based apps (Discord) need it.
pub const LOOPBACK_COMMAND: &str =
    "sudo modprobe v4l2loopback video_nr=10 card_label=Lucentia exclusive_caps=1";

/// Whether the v4l2loopback module exists for the running kernel.
pub fn loopback_module_available() -> bool {
    let Ok(release) = std::fs::read_to_string("/proc/sys/kernel/osrelease") else {
        return false;
    };
    let dir = std::path::Path::new("/lib/modules").join(release.trim());
    ["modules.dep", "modules.builtin"].iter().any(|f| {
        std::fs::read_to_string(dir.join(f)).is_ok_and(|t| t.contains("v4l2loopback"))
    })
}

/// v4l2loopback devices on this machine (virtual video devices, not real cameras).
pub fn loopback_devices() -> Vec<(PathBuf, String)> {
    let Ok(dir) = std::fs::read_dir("/sys/class/video4linux") else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, String)> = dir
        .filter_map(|e| e.ok())
        .filter(|e| {
            // Loopback devices are virtual: they live under /sys/devices/virtual.
            std::fs::canonicalize(e.path())
                .is_ok_and(|p| p.to_string_lossy().contains("/devices/virtual/"))
        })
        .map(|e| {
            let name = std::fs::read_to_string(e.path().join("name"))
                .map(|n| n.trim().to_string())
                .unwrap_or_default();
            (PathBuf::from("/dev").join(e.file_name()), name)
        })
        .collect();
    found.sort();
    found
}

impl Session {
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle {
            pipeline: self.pipeline.downgrade(),
            stopping: self.stopping.clone(),
        }
    }

    pub fn is_recording(&self) -> bool {
        self.recording.lock().unwrap().is_some()
    }

    /// Start recording to the file in `plan`, without interrupting the capture.
    pub fn start_recording(&self, opts: &Options, plan: Plan) -> Result<(), String> {
        let mut active = self.recording.lock().unwrap();
        if active.is_some() {
            return Err("already recording".into());
        }
        if let Some(dir) = plan
            .final_path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("can't create {}: {e}", dir.display()))?;
        }

        let n = self.next_recording.fetch_add(1, Ordering::Relaxed);
        let bin = gst::Bin::with_name(&format!("recording-{n}"));
        // Report this bin's end of stream on the bus, so we know when its file is done.
        bin.set_property("message-forward", true);

        let mux = make("matroskamux")?;
        if mux.find_property("writing-app").is_some() {
            mux.set_property("writing-app", "Lucentia");
        }
        let sink = make("filesink")?;
        sink.set_property("location", plan.mkv_path.to_string_lossy().to_string());
        bin.add_many([&mux, &sink]).map_err(|e| e.to_string())?;
        mux.link(&sink)
            .map_err(|_| "could not connect the muxer to the file")?;

        // Each stream: input queue -> encoder -> named track on the muxer. The video
        // queue is unbounded so frames wait in memory while the encoder starts up.
        let mut inputs: Vec<(gst::Element, gst::Element, &str)> = Vec::new();
        let mut video = vec![unbounded_queue()?];
        if let Some(fps) = plan.video.output_fps {
            // Keep every Nth frame: drops only, never duplicates.
            let rate = make("videorate")?;
            rate.set_property("drop-only", true);
            rate.set_property("max-rate", fps as i32);
            // Start counting from the first frame, not from the session's start.
            rate.set_property("skip-to-first", true);
            video.push(rate);
        }
        video.extend(encode::video_chain(&plan.video)?);
        let video_queue = video[0].clone();
        let encoder = video
            .iter()
            .filter_map(|e| e.factory().map(|f| f.name().to_string()))
            .find(|n| n.ends_with("enc"))
            .unwrap_or_else(|| "raw (no encoder)".into());
        add_to_bin(&bin, &video, "video encoder")?;
        link_to_mux(video.last().unwrap(), &mux, "Video")?;
        inputs.push((self.video_tee.clone(), video[0].clone(), "video"));

        let audio_track = |tee: &gst::Element,
                           title: &'static str|
         -> Result<(gst::Element, gst::Element, &'static str), String> {
            let mut chain = vec![make("queue")?];
            chain.extend(encode::audio_chain(opts.audio_codec, opts.audio_kbps)?);
            add_to_bin(&bin, &chain, title)?;
            link_to_mux(chain.last().unwrap(), &mux, title)?;
            Ok((tee.clone(), chain[0].clone(), title))
        };
        if let Some(t) = &self.game_tee {
            inputs.push(audio_track(t, "Game")?);
        }
        if let Some(t) = &self.mic_tee {
            inputs.push(audio_track(t, "Mic")?);
        }

        // Inputs on the bin's edge, so the session's tees can connect to it.
        let mut ghosts = Vec::new();
        for (tee, first, what) in &inputs {
            let target = first.static_pad("sink").unwrap();
            let ghost = gst::GhostPad::builder_with_target(&target)
                .map_err(|e| e.to_string())?
                .name(*what)
                .build();
            bin.add_pad(&ghost).map_err(|e| e.to_string())?;
            ghosts.push((tee.clone(), ghost.upcast::<gst::Pad>(), *what));
        }

        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        bin.sync_state_with_parent()
            .map_err(|_| "could not start the recorder".to_string())?;

        // Timestamps: shift every stream so the file starts at zero instead of at the
        // session's running time, and drop audio until the first video frame arrives,
        // so the file doesn't open with a frozen or blank picture.
        let now = self
            .pipeline
            .current_running_time()
            .map(|t| t.nseconds() as i64)
            .unwrap_or(0);
        let first_video = Arc::new(AtomicU64::new(u64::MAX));
        let mut links = Vec::new();
        for (tee, ghost, what) in ghosts {
            let extra = if what == "Mic" {
                self.mic_offset_ms * 1_000_000
            } else {
                0
            };
            ghost.set_offset(-now + extra);
            let fv = first_video.clone();
            let is_video = what == "video";
            // The drop probe sits on the tee's pad, not the ghost pad: dropping on a
            // pad with an offset trips a GStreamer refcount assertion.
            let tee_pad = tee
                .request_pad_simple("src_%u")
                .ok_or("the capture refused another output")?;
            tee_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(pts) = info.buffer().and_then(|b| b.pts()) else {
                    return gst::PadProbeReturn::Ok;
                };
                // Anything from before Record was pressed would land before the
                // file's start. The muxer throws those away, and if the first one
                // was the keyframe, the video decodes as gray smears until the next
                // keyframe (about 2 s). The card's frames arrive a little late, so
                // there are usually a few of these; drop them before the encoder.
                if (pts.nseconds() as i64) + extra < now {
                    return gst::PadProbeReturn::Drop;
                }
                if is_video {
                    let _ = fv.compare_exchange(
                        u64::MAX,
                        pts.nseconds(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    gst::PadProbeReturn::Ok
                } else {
                    let start = fv.load(Ordering::Acquire);
                    if start == u64::MAX || pts.nseconds() < start {
                        gst::PadProbeReturn::Drop
                    } else {
                        gst::PadProbeReturn::Ok
                    }
                }
            });
            tee_pad
                .link(&ghost)
                .map_err(|e| format!("could not attach the recorder ({what}): {e:?}"))?;
            links.push((tee, tee_pad, ghost));
        }

        *active = Some(ActiveRecording {
            bin,
            links,
            plan,
            started: Instant::now(),
            lost_at_start: self.stats.lost.load(Ordering::Relaxed),
            video_queue,
            encoder,
        });
        Ok(())
    }

    /// Detach the recording from the capture and end its streams. The capture keeps
    /// running. Call `wait_recording` afterwards (it can take a moment for the encoder
    /// to write its last frames), then `finish_recording`.
    pub fn stop_recording(&self) -> Option<StoppedRecording> {
        let rec = self.recording.lock().unwrap().take()?;
        for (_, tee_pad, ghost) in &rec.links {
            let ghost = ghost.clone();
            // Wait until no buffer is in flight on this branch, then unlink it and end
            // its stream. Nothing upstream is paused or reopened.
            tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                let _ = pad.unlink(&ghost);
                ghost.send_event(gst::event::Eos::new());
                gst::PadProbeReturn::Remove
            });
        }
        Some(StoppedRecording {
            bin: rec.bin,
            links: rec.links,
            lost: self
                .stats
                .lost
                .load(Ordering::Relaxed)
                .saturating_sub(rec.lost_at_start),
            plan: rec.plan,
            interrupted: false,
        })
    }

    /// Take the recording after the session failed; its file is as complete as the
    /// error allowed (an MKV is still playable).
    pub fn abandon_recording(&self) -> Option<StoppedRecording> {
        let mut s = self.stop_recording()?;
        s.interrupted = true;
        Some(s)
    }

    /// Wait until a stopped recording's file is completely written, then remove it
    /// from the pipeline. Returns false on timeout (the file is still playable).
    pub fn wait_recording(&self, rec: &StoppedRecording, timeout: Duration) -> bool {
        let name = rec.bin.name().to_string();
        let (lock, cvar) = &*self.finished_bins;
        let done = if rec.interrupted {
            false
        } else {
            let guard = lock.lock().unwrap();
            let (guard, _) = cvar
                .wait_timeout_while(guard, timeout, |done| !done.contains(&name))
                .unwrap();
            guard.contains(&name)
        };
        let _ = rec.bin.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&rec.bin);
        for (tee, tee_pad, _) in &rec.links {
            tee.release_request_pad(tee_pad);
        }
        done
    }

    /// Turn the virtual camera on (or switch it), or off with None. Like a
    /// recording, it attaches to the running capture without reopening the card.
    pub fn set_virtual_camera(&self, target: Option<VirtualCamera>) -> Result<(), String> {
        let mut current = self.virtual_camera.lock().unwrap();
        if let Some((bin, tee_pad)) = current.take() {
            let ghost = bin.static_pad("sink");
            let (bin2, pipeline, tee) = (bin.clone(), self.pipeline.clone(), self.video_tee.clone());
            tee_pad.add_probe(gst::PadProbeType::IDLE, move |pad, _| {
                if let Some(g) = &ghost {
                    let _ = pad.unlink(g);
                }
                let (bin, pipeline, tee, pad) = (bin2.clone(), pipeline.clone(), tee.clone(), pad.clone());
                // Tear down off the streaming thread.
                std::thread::spawn(move || {
                    let _ = bin.set_state(gst::State::Null);
                    let _ = pipeline.remove(&bin);
                    tee.release_request_pad(&pad);
                });
                gst::PadProbeReturn::Remove
            });
            log::debug("virtual camera off");
        }
        let Some(target) = target else {
            return Ok(());
        };

        let bin = gst::Bin::with_name("virtual-camera");
        // Leaky: if a viewer stalls, drop frames for it instead of holding up the
        // preview and recording.
        let q = make("queue")?;
        q.set_property_from_str("leaky", "downstream");
        q.set_property("max-size-buffers", 3u32);
        q.set_property("max-size-bytes", 0u32);
        q.set_property("max-size-time", 0u64);
        let convert = make("videoconvert")?;
        let scale = make("videoscale")?;
        let caps = make("capsfilter")?;
        // At most 1080p: what Discord and browsers handle well, and light to convert.
        let mode = &self.plan.mode;
        let (w, h) = if mode.width > 1920 {
            (1920, (1920 * mode.height / mode.width.max(1)) & !1)
        } else {
            (mode.width, mode.height)
        };
        let format = match target {
            // v4l2loopback readers (Chromium, Discord) like YUY2 best.
            VirtualCamera::Loopback(_) => "YUY2",
            VirtualCamera::PipeWire => "NV12",
        };
        caps.set_property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("format", format)
                .field("width", w)
                .field("height", h)
                .build(),
        );
        let sink = match &target {
            VirtualCamera::PipeWire => {
                let s = make("pipewiresink")?;
                if s.find_property("mode").is_some() {
                    s.set_property_from_str("mode", "provide");
                }
                s.set_property("client-name", "Lucentia");
                s.set_property(
                    "stream-properties",
                    gst::Structure::builder("props")
                        .field("media.class", "Video/Source")
                        .field("media.role", "Camera")
                        .field("node.name", "lucentia")
                        .field("node.description", "Lucentia (capture card)")
                        .build(),
                );
                s
            }
            VirtualCamera::Loopback(dev) => {
                let s = make("v4l2sink")?;
                s.set_property("device", dev.to_string_lossy().to_string());
                s
            }
        };
        sink.set_property("sync", false);
        sink.set_property("async", false);
        let chain = [q.clone(), convert, scale, caps, sink];
        bin.add_many(&chain).map_err(|e| e.to_string())?;
        gst::Element::link_many(&chain).map_err(|_| "could not build the virtual camera")?;
        let ghost = gst::GhostPad::builder_with_target(&q.static_pad("sink").unwrap())
            .map_err(|e| e.to_string())?
            .name("sink")
            .build();
        bin.add_pad(&ghost).map_err(|e| e.to_string())?;
        self.pipeline.add(&bin).map_err(|e| e.to_string())?;
        if bin.sync_state_with_parent().is_err() {
            let _ = bin.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&bin);
            return Err(match target {
                VirtualCamera::PipeWire => "could not create the PipeWire camera. Is PipeWire running?".into(),
                VirtualCamera::Loopback(d) => format!("could not open {} (v4l2loopback)", d.display()),
            });
        }
        let tee_pad = self
            .video_tee
            .request_pad_simple("src_%u")
            .ok_or("the capture refused another output")?;
        tee_pad
            .link(&ghost)
            .map_err(|e| format!("could not attach the virtual camera: {e:?}"))?;
        log::debug(&format!("virtual camera on: {target:?} at {w}x{h} {format}"));
        *current = Some((bin, tee_pad));
        Ok(())
    }

    /// Change the size of the frames sent to the window (bigger for the pop-out
    /// video window), without restarting anything.
    pub fn set_preview_width(&self, max_width: i32) {
        let (w, h) = preview::preview_size_max(&self.plan.mode, max_width);
        for (name, gl) in [("preview-gl-size", true), ("preview-size", false)] {
            if let Some(f) = self.pipeline.by_name(name) {
                let b = gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .field("width", w as i32)
                    .field("height", h as i32);
                let caps = if gl {
                    b.features(["memory:GLMemory"]).build()
                } else {
                    b.build()
                };
                f.set_property("caps", caps);
            }
        }
        log::debug(&format!("preview frames now {w}x{h}"));
    }

    /// Latest peak levels in dB: (game, mic).
    pub fn levels(&self) -> (f32, f32) {
        (
            f32::from_bits(self.levels[0].load(Ordering::Relaxed)),
            f32::from_bits(self.levels[1].load(Ordering::Relaxed)),
        )
    }

    fn update_level(&self, msg: &gst::Message, s: &gst::StructureRef) {
        let slot = match msg.src().map(|o| o.name()) {
            Some(n) if n == "level-game" => 0,
            Some(n) if n == "level-mic" => 1,
            _ => return,
        };
        // "peak" holds one value per channel; the meter shows the loudest.
        let peak = s
            .value("peak")
            .ok()
            .and_then(|v| v.get::<gst::glib::ValueArray>().ok())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.get::<f64>().ok())
                    .fold(SILENT_DB as f64, f64::max)
            })
            .unwrap_or(SILENT_DB as f64);
        self.levels[slot].store((peak as f32).to_bits(), Ordering::Relaxed);
    }

    /// Let go of the card, so a new session can open it right away.
    pub fn release(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    /// Run until the session ends (stopped, --duration reached, or an error), calling
    /// `report` with fresh stats every `every`.
    pub fn watch(
        &self,
        duration: Option<f64>,
        every: Duration,
        mut report: impl FnMut(&Stats),
    ) -> Result<(), String> {
        let bus = self.pipeline.bus().ok_or("pipeline has no bus")?;
        let mut last = (Instant::now(), 0u64);
        let mut cpu_prev = cpu_seconds();
        let cpus = std::thread::available_parallelism()
            .map(|n| n.get() as f64)
            .unwrap_or(1.0);
        loop {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Eos(_) => return Ok(()),
                    MessageView::Element(e) => {
                        // Level meter readings (no `continue`: the stats and the
                        // --duration check below must still run).
                        if let Some(s) = e.structure().filter(|s| s.name() == "level") {
                            self.update_level(&msg, s);
                        }
                        // A recording bin's own end of stream: its file is done.
                        if let Some(s) = e.structure() {
                            if s.name() == "GstBinForwarded" {
                                if let Ok(inner) = s.get::<gst::Message>("message") {
                                    if inner.type_() == gst::MessageType::Eos {
                                        if let Some(src) = msg.src() {
                                            let (lock, cvar) = &*self.finished_bins;
                                            lock.lock().unwrap().push(src.name().to_string());
                                            cvar.notify_all();
                                        }
                                    }
                                }
                            }
                        }
                    }
                    MessageView::Error(e) => {
                        let text = e.error().to_string();
                        if text.contains("window was closed") {
                            if self.stop_handle().stop() {
                                println!("{}", log::yellow("preview closed, finishing up"));
                            }
                            continue;
                        }
                        let debug = e.debug().map(|d| d.to_string()).unwrap_or_default();
                        log::debug(&format!(
                            "from {:?}: {debug}",
                            e.src().map(|s| s.path_string())
                        ));
                        return Err(if self.is_recording() {
                            format!("{text} (the recording so far is still playable)")
                        } else {
                            text
                        });
                    }
                    MessageView::Warning(w) => log::warn(&w.error().to_string()),
                    _ => {}
                }
            }

            if let Some(d) = duration {
                if self.started.elapsed().as_secs_f64() >= d && self.stop_handle().stop() {
                    println!("{} reached --duration", log::dim("stopping:"));
                }
            }

            if last.0.elapsed() >= every && !self.stopping.load(Ordering::Relaxed) {
                let frames = self.stats.frames.load(Ordering::Relaxed);
                let fps = (frames - last.1) as f64 / last.0.elapsed().as_secs_f64();
                let lost_total = self.stats.lost.load(Ordering::Relaxed);
                let (cpu_now, cpu_pct) = {
                    let now = cpu_seconds();
                    let wall = last.0.elapsed().as_secs_f64().max(0.001);
                    let pct = match (now, cpu_prev) {
                        (Some(n), Some(p)) => (n - p) / wall / cpus * 100.0,
                        _ => 0.0,
                    };
                    (now, pct)
                };
                cpu_prev = cpu_now;
                let mut st = match &*self.recording.lock().unwrap() {
                    Some(rec) => {
                        let elapsed = rec.started.elapsed().as_secs_f64();
                        let size = std::fs::metadata(&rec.plan.mkv_path)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let per_sec = size as f64 / elapsed.max(1.0);
                        // Only estimate once data is actually reaching the file.
                        let drive_left = estimate::free_space(&rec.plan.mkv_path)
                            .filter(|_| size > 0 && per_sec > 1000.0)
                            .map(|free| free as f64 / per_sec);
                        Stats {
                            elapsed,
                            size,
                            mbps: size as f64 * 8.0 / 1e6 / elapsed.max(1.0),
                            fps,
                            lost: lost_total.saturating_sub(rec.lost_at_start),
                            drive_left,
                            recording: true,
                            encoder_backlog: rec
                                .video_queue
                                .property::<u64>("current-level-time")
                                as f64
                                / 1e9,
                            encoder: Some(rec.encoder.clone()),
                            ..Default::default()
                        }
                    }
                    None => Stats {
                        elapsed: self.started.elapsed().as_secs_f64(),
                        fps,
                        lost: lost_total,
                        ..Default::default()
                    },
                };
                st.damaged = self.stats.damaged.load(Ordering::Relaxed);
                st.gaps = self.stats.gaps.load(Ordering::Relaxed);
                st.worst_gap_ms = self.stats.worst_gap_ns.load(Ordering::Relaxed) as f64 / 1e6;
                st.lost_total = lost_total;
                st.cpu_pct = cpu_pct;
                report(&st);
                last = (Instant::now(), frames);
            }
        }
    }
}

/// CPU time this process has used so far, in seconds (Linux).
fn cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // Fields after the command name, which is in parentheses and may contain spaces.
    let rest = stat.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    // SAFETY: sysconf with a valid name has no preconditions.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    Some((utime + stime) / ticks.max(1.0))
}

/// Lower this thread's CPU and disk priority, so background work (finishing a file)
/// never makes the capture, the preview or a game stutter. Threads started from here
/// (GStreamer's) inherit it.
fn lower_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // SAFETY: plain syscalls on the calling thread; failures are harmless.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS, tid, 10);
        const IOPRIO_WHO_PROCESS: libc::c_long = 1;
        const IOPRIO_CLASS_IDLE: libc::c_long = 3;
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS,
            tid as libc::c_long,
            IOPRIO_CLASS_IDLE << 13,
        );
    }
}

fn adds_mix(plan: &Plan, opts: &Options) -> bool {
    opts.mix_track && opts.mic.is_some() && plan.game_audio
}

/// Whether finishing has real work to do (mix the audio, convert to MP4), rather
/// than just closing the MKV.
pub fn needs_stitching(plan: &Plan, opts: &Options) -> bool {
    plan.container == Container::Mp4 || adds_mix(plan, opts)
}

/// Finish a recording's file after it stopped: mix Game + Mic and/or convert to MP4.
/// Runs at low priority so it never competes with the capture.
pub fn finish_recording(
    rec: StoppedRecording,
    opts: &Options,
    progress: &dyn Fn(f64),
) -> Finished {
    lower_priority();
    let plan = &rec.plan;
    let size = std::fs::metadata(&plan.mkv_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let add_mix = adds_mix(plan, opts);
    if needs_stitching(plan, opts) {
        let mix = add_mix.then_some((opts.audio_codec, opts.audio_kbps));
        match remux::finalize(
            &plan.mkv_path,
            &plan.final_path,
            plan.container == Container::Mp4,
            mix,
            progress,
        ) {
            Ok(()) => {
                return Finished {
                    size: std::fs::metadata(&plan.final_path)
                        .map(|m| m.len())
                        .unwrap_or(0),
                    path: plan.final_path.clone(),
                    lost: rec.lost,
                    finish_error: None,
                }
            }
            Err(e) => {
                return Finished {
                    path: plan.mkv_path.clone(),
                    size,
                    lost: rec.lost,
                    finish_error: Some(e),
                }
            }
        }
    }
    Finished {
        path: plan.mkv_path.clone(),
        size,
        lost: rec.lost,
        finish_error: None,
    }
}

/// The `lucentia record` command: plan, print, record until Ctrl+C or --duration,
/// then finish the file.
pub fn run(card: Option<&CaptureDevice>, opts: &Options) -> Result<(), String> {
    let plan = plan(card, opts)?;
    for w in &plan.warnings {
        log::warn(w);
    }
    print_plan(card, &plan, opts);
    check_disk(&plan.final_path, plan.est, plan.video.codec);

    let session = Arc::new(start(card, opts, plan.clone())?);
    session.start_recording(opts, plan)?;

    // Ctrl+C (or --duration) ends the session; a second Ctrl+C gives up waiting.
    let stop = session.stop_handle();
    let _ = ctrlc::set_handler(move || {
        if !stop.stop() {
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
    });
    println!(
        "{} {}  (Ctrl+C to stop)",
        log::green("recording"),
        session.plan.mkv_path.display()
    );

    let mut warned_lost = 0u64;
    let watched = session.watch(opts.duration, Duration::from_secs(5), |st| {
        let left = st
            .drive_left
            .map(|s| format!(", {} left on drive", estimate::human_duration(s)))
            .unwrap_or_default();
        println!(
            "  {}  {}  {:.1} Mbit/s  {:.2} fps{left}",
            estimate::human_duration(st.elapsed),
            estimate::human_bytes(st.size as f64),
            st.mbps,
            st.fps,
        );
        if st.lost > warned_lost {
            log::warn(&format!("{} frame(s) lost so far", st.lost));
            warned_lost = st.lost;
        }
    });

    // The session's end of stream also ended the recording's streams, so its file is
    // complete; take it out and finish it.
    let rec = if watched.is_ok() {
        session.stop_recording()
    } else {
        session.abandon_recording()
    };
    session.release();
    let Some(rec) = rec else {
        return watched;
    };
    let _ = session.wait_recording(&rec, Duration::from_secs(1));

    let add_mix = opts.mix_track && opts.mic.is_some() && rec.plan.game_audio;
    if add_mix || rec.plan.container == Container::Mp4 {
        println!(
            "{} {} (video is copied, not re-encoded)",
            log::dim("finishing:"),
            match (add_mix, rec.plan.container) {
                (true, Container::Mp4) => "mixing Game + Mic and converting to MP4",
                (true, Container::Mkv) => "mixing Game + Mic",
                _ => "converting to MP4",
            }
        );
    }
    let bar = log::Progress::new("stitching:");
    let f = finish_recording(rec, opts, &|d| bar.update(d));
    bar.done();
    if f.lost > 0 {
        log::warn(&format!(
            "{} frame(s) were lost during the recording",
            f.lost
        ));
    } else {
        log::ok("no frames lost");
    }
    if let Some(e) = &f.finish_error {
        log::warn(&format!(
            "finishing failed ({e}); your recording is safe as MKV with separate tracks"
        ));
    }
    log::ok(&format!(
        "saved {} ({})",
        f.path.display(),
        estimate::human_bytes(f.size as f64)
    ));
    watched
}

/// The parts of an audio source that can be changed while running.
struct AudioSource {
    volume: gst::Element,
    /// The speakers branch, when this source is being played.
    listen: Option<gst::Element>,
    /// Recordings attach here.
    tee: gst::Element,
}

/// Audio source -> 48 kHz stereo -> volume -> tee -> [speakers].
/// Echo cancellation for the mic: WebRTC's AEC3 (the `sonora` crate, pure Rust).
/// The game audio is the reference for what the speakers play; whatever of it the
/// mic picks up is subtracted from the mic. Both streams are cut into 10 ms frames of
/// 16-bit stereo at 48 kHz, which is what AEC3 works on.
///
/// Returns the elements to put in the mic's path. The reference branch is attached
/// to the game tee here.
fn echo_canceller(
    pipeline: &gst::Pipeline,
    game_tee: &gst::Element,
) -> Result<Vec<gst::Element>, String> {
    const CHANNELS: usize = 2;
    const FRAME: usize = 480 * CHANNELS; // 10 ms at 48 kHz

    let config = sonora::StreamConfig::new(48_000, CHANNELS as u16);
    let apm = sonora::AudioProcessing::builder()
        .config(sonora::Config {
            echo_canceller: Some(sonora::config::EchoCanceller::default()),
            ..Default::default()
        })
        .capture_config(config)
        .render_config(config)
        .build();
    let apm = Arc::new(Mutex::new(apm));

    let frames = |what: &str| -> Result<Vec<gst::Element>, String> {
        let caps = make("capsfilter")?;
        caps.set_property(
            "caps",
            gst::Caps::builder("audio/x-raw")
                .field("format", "S16LE")
                .field("layout", "interleaved")
                .field("rate", 48_000i32)
                .field("channels", CHANNELS as i32)
                .build(),
        );
        let split = make("audiobuffersplit").map_err(|_| {
            format!("echo cancellation needs the audiobuffersplit plugin ({what})")
        })?;
        split.set_property("output-buffer-duration", gst::Fraction::new(1, 100));
        Ok(vec![make("audioconvert")?, make("audioresample")?, caps, split])
    };

    // Reference: the game audio, as it's captured.
    let q = make("queue")?;
    q.set_property_from_str("leaky", "downstream");
    let sink = make("fakesink")?;
    sink.set_property("sync", false);
    sink.set_property("async", false);
    let mut branch = vec![q];
    branch.extend(frames("game")?);
    branch.push(sink.clone());
    add_chain(pipeline, &branch, "echo reference")?;
    {
        let apm = apm.clone();
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(map) = info.buffer().and_then(|b| b.map_readable().ok()) {
                    if map.len() == FRAME * 2 {
                        let samples: Vec<i16> = map
                            .chunks_exact(2)
                            .map(|b| i16::from_le_bytes([b[0], b[1]]))
                            .collect();
                        let mut out = vec![0i16; FRAME];
                        let _ = apm.lock().unwrap().process_render_i16(&samples, &mut out);
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }
    game_tee
        .link(&branch[0])
        .map_err(|_| "could not connect the echo reference")?;

    // The mic: cleaned in place, frame by frame.
    let mic = frames("mic")?;
    {
        mic.last()
            .unwrap()
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                let Some(buf) = info.buffer_mut() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Ok(mut map) = buf.make_mut().map_writable() else {
                    return gst::PadProbeReturn::Ok;
                };
                if map.len() != FRAME * 2 {
                    return gst::PadProbeReturn::Ok; // a partial last frame
                }
                let input: Vec<i16> = map
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                let mut output = vec![0i16; FRAME];
                if apm.lock().unwrap().process_capture_i16(&input, &mut output).is_ok() {
                    for (b, s) in map.chunks_exact_mut(2).zip(&output) {
                        b.copy_from_slice(&s.to_le_bytes());
                    }
                }
                gst::PadProbeReturn::Ok
            });
    }
    log::debug("mic echo cancellation on (AEC3)");
    Ok(mic)
}

fn add_audio_source(
    pipeline: &gst::Pipeline,
    src: gst::Element,
    what: &str,
    volume_pct: f64,
    listen: bool,
    // Processing for this source (the mic's echo canceller), before its volume.
    dsp: Option<Vec<gst::Element>>,
) -> Result<AudioSource, String> {
    let caps = make("capsfilter")?;
    caps.set_property(
        "caps",
        gst::Caps::builder("audio/x-raw")
            .field("rate", 48_000i32)
            .field("channels", 2i32)
            .build(),
    );
    let vol = make("volume")?;
    vol.set_property("volume", volume_pct / 100.0);
    let tee = new_tee()?;
    let mut chain = vec![
        src,
        make("queue")?,
        make("audioconvert")?,
        make("audioresample")?,
        caps,
    ];
    if let Some(d) = dsp {
        chain.extend(d);
        chain.push(make("audioconvert")?);
    }
    chain.push(vol.clone());
    // Peak level for the meters, 20 times a second.
    let level = make("level")?;
    level.set_property("name", format!("level-{}", if what == "mic" { "mic" } else { "game" }));
    level.set_property("interval", 50_000_000u64);
    level.set_property("post-messages", true);
    chain.push(level);
    chain.push(tee.clone());
    add_chain(pipeline, &chain, what)?;

    let mut speakers = None;
    if listen {
        // Its own volume element mutes the speakers without touching recordings.
        // The speakers play whatever arrives, without matching timestamps to the
        // pipeline clock. The card's audio and the speakers run on different clocks,
        // and when the sink tried to keep them lined up (or frames arrived late under
        // load) it got stuck dropping and padding audio: choppy until a mute reset it.
        // A short leaky queue keeps the delay low if the card runs a hair fast.
        let q = make("queue")?;
        q.set_property_from_str("leaky", "downstream");
        q.set_property("max-size-buffers", 0u32);
        q.set_property("max-size-bytes", 0u32);
        q.set_property("max-size-time", 150_000_000u64);
        let mute = make("volume")?;
        let sink = make("autoaudiosink")?;
        sink.set_property("sync", false);
        let out = [q, mute.clone(), make("audioconvert")?, sink];
        add_chain(pipeline, &out, "speakers")?;
        tee.link(&out[0])
            .map_err(|_| "could not connect the speakers")?;
        speakers = Some(mute);
    }
    Ok(AudioSource {
        volume: vol,
        listen: speakers,
        tee,
    })
}

fn add_to_bin(bin: &gst::Bin, chain: &[gst::Element], what: &str) -> Result<(), String> {
    bin.add_many(chain)
        .map_err(|e| format!("could not build the {what} recorder: {e}"))?;
    if chain.len() > 1 {
        gst::Element::link_many(chain)
            .map_err(|_| format!("could not connect the {what} recorder"))?;
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
    let caps = make("capsfilter")?;
    caps.set_property("caps", preview::mode_caps(mode));
    let src = make("videotestsrc")?;
    src.set_property("is-live", true);
    let mut chain = vec![src.clone(), caps.clone()];
    match std::env::var("LUCENTIA_TEST_IMAGE") {
        // LUCENTIA_TEST_IMAGE=shot.png shows a still picture over the test pattern
        // (for UI screenshots that look like real use).
        Ok(path) => {
            src.set_property_from_str("pattern", "black");
            let overlay = make("gdkpixbufoverlay")?;
            overlay.set_property("location", path);
            overlay.set_property("overlay-width", mode.width);
            overlay.set_property("overlay-height", mode.height);
            chain.push(overlay);
        }
        Err(_) => src.set_property_from_str("pattern", "ball"),
    }

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
    /// Frames the card flagged as damaged (dropped before anything sees them).
    damaged: AtomicU64,
    /// Times frames went missing, and the longest gap in nanoseconds.
    gaps: AtomicU64,
    worst_gap_ns: AtomicU64,
    last_pts: Mutex<Option<gst::ClockTime>>,
    frame_ns: f64,
}

impl FrameStats {
    fn new(fps: f64) -> Self {
        FrameStats {
            frames: AtomicU64::new(0),
            lost: AtomicU64::new(0),
            damaged: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            worst_gap_ns: AtomicU64::new(0),
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
                self.gaps.fetch_add(1, Ordering::Relaxed);
                self.worst_gap_ns.fetch_max(gap as u64, Ordering::Relaxed);
                log::debug(&format!(
                    "gap of {:.1} ms: {missing} frame(s) lost",
                    gap / 1e6
                ));
            }
        }
        *last = Some(pts);
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

pub(crate) fn local_timestamp() -> String {
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

fn print_plan(card: Option<&CaptureDevice>, plan: &Plan, opts: &Options) {
    let (mode, hdr, v, audio_tracks, container, path, est) = (
        &plan.mode,
        plan.hdr,
        &plan.video,
        plan.audio_tracks,
        plan.container,
        &plan.final_path,
        plan.est,
    );
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
    if let Some(f) = v.output_fps {
        println!("{} {f} fps (every {} frame(s) of the capture)", log::bold("recording:"),
            preview::fmt_fps(mode.fps() / f as f64));
    }
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

    /// Preview-only session sending frames to a callback, the way the app window uses
    /// it: HDR test pattern in, tone-mapped 1280x720 RGBA frames out. Needs OpenGL, so
    /// it's opt-in: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn app_preview_delivers_frames() {
        gst::init().unwrap();
        let got: Arc<Mutex<Vec<(u32, u32, u8)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_got = got.clone();
        let opts = Options {
            capture: preview::Options {
                mode: Some((1920, 1080, 30.0)),
                hdr: true,
                mjpeg: false,
                hdr_signal: false,
                audio: true,
            },
            codec: None,
            rate: None,
            keyframe_secs: 2.0,
            analysis: false,
            record_fps: None,
            audio_codec: AudioCodec::Aac,
            audio_kbps: 320,
            mic: None,
            mic_offset_ms: 0,
            mix_track: false,
            game_volume: 100.0,
            mic_volume: 100.0,
            container: Container::Mkv,
            output: None,
            duration: Some(2.0),
            preview: false,
            test_source: true,
            record: false,
            listen_muted: false,
            mic_muted: false,
            echo_cancel: false,
            frames: Some(Arc::new(move |f: preview::Frame| {
                let max = f.rgba.iter().step_by(4).copied().max().unwrap_or(0);
                assert_eq!(f.rgba.len(), (f.width * f.height * 4) as usize);
                sink_got.lock().unwrap().push((f.width, f.height, max));
            })),
        };
        let plan = plan(None, &opts).unwrap();
        let session = start(None, &opts, plan).unwrap();
        session
            .watch(opts.duration, Duration::from_secs(1), |_| {})
            .unwrap();
        assert!(
            session.stop_recording().is_none(),
            "preview-only writes no file"
        );
        let got = got.lock().unwrap();
        assert!(got.len() > 10, "only {} frames arrived", got.len());
        assert_eq!((got[0].0, got[0].1), (1280, 720));
        assert!(
            got.iter().any(|f| f.2 > 200),
            "frames look washed out / dark"
        );
    }

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
        assert_eq!(default_kbps(&m(1920, 1080, 60), Codec::H264), 40_000);
        assert_eq!(default_kbps(&m(1920, 1080, 60), Codec::Hevc), 28_000);
        assert_eq!(default_kbps(&m(1920, 1080, 60), Codec::Av1), 24_000);
        assert_eq!(default_kbps(&m(3840, 2160, 60), Codec::H264), 160_000);
        assert_eq!(default_kbps(&m(1280, 720, 30), Codec::H264), 10_000);
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
    /// AEC3 removes a delayed copy of the game audio from the mic. Render: a mix of
    /// tones standing in for game audio. Capture: that, 40 ms later and quieter (the
    /// speakers into the mic). After it adapts, the echo should be mostly gone.
    #[test]
    fn echo_canceller_removes_game_audio() {
        const FRAME: usize = 960; // 10 ms, stereo, 48 kHz
        let config = sonora::StreamConfig::new(48_000, 2);
        let mut apm = sonora::AudioProcessing::builder()
            .config(sonora::Config {
                echo_canceller: Some(sonora::config::EchoCanceller::default()),
                ..Default::default()
            })
            .capture_config(config)
            .render_config(config)
            .build();
        // Broadband noise (like music or game sound), the same per sample index.
        let game = |n: usize| -> i16 {
            let mut x = (n / 2) as u64 ^ 0x9E37_79B9_7F4A_7C15;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 31;
            ((x % 16_000) as i32 - 8_000) as i16
        };
        let delay = 40 * 96; // 40 ms of stereo samples
        let (mut before, mut after) = (0f64, 0f64);
        let mut out = vec![0i16; FRAME];
        for f in 0..600 {
            let render: Vec<i16> = (0..FRAME).map(|i| game(f * FRAME + i)).collect();
            let capture: Vec<i16> = (0..FRAME)
                .map(|i| {
                    let n = f * FRAME + i;
                    if n >= delay { game(n - delay) / 2 } else { 0 }
                })
                .collect();
            apm.process_render_i16(&render, &mut out).unwrap();
            apm.process_capture_i16(&capture, &mut out).unwrap();
            if f >= 400 {
                before += capture.iter().map(|&x| (x as f64).powi(2)).sum::<f64>();
                after += out.iter().map(|&x| (x as f64).powi(2)).sum::<f64>();
            }
        }
        let reduction_db = 10.0 * (before / after.max(1.0)).log10();
        assert!(reduction_db > 15.0, "echo only reduced by {reduction_db:.1} dB");
    }

}
