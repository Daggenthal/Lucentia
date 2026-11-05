//! The app window: live preview, settings, Record/Stop.
//!
//! The window always runs a session: preview-only while idle, preview + recording
//! while recording. Changing the device or mode restarts the preview. Pressing Record
//! restarts the session with recording on (the preview blinks for about a second; the
//! recording itself isn't affected).
//!
//! Sessions run on a worker thread. Frames and stats come back to the window through
//! Slint's event loop, which is the only place UI objects may be touched.

use gstreamer::prelude::DeviceExt;
use slint::{ComponentHandle, ModelRc, SharedPixelBuffer, SharedString, VecModel};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::devices::{self, CaptureDevice, Kind, Mode};
use crate::encode::{AudioCodec, Codec, RateControl};
use crate::estimate;
use crate::log;
use crate::preview::{self, Frame};
use crate::record::{self, Container, StopHandle};
use std::sync::mpsc;

slint::include_modules!();

/// One entry in the frame rate menu for the selected resolution.
#[derive(Clone)]
struct RateChoice {
    fps: f64,
    /// Lossless at this rate; otherwise only MJPEG.
    lossless: bool,
    /// A 10-bit (HDR) lossless mode exists at this rate.
    hdr: bool,
    /// Record at this lower rate, keeping every Nth captured frame (40 from 120).
    record_fps: Option<u32>,
}

impl RateChoice {
    /// The frame rate the file ends up with.
    fn shown(&self) -> f64 {
        self.record_fps.map(|f| f as f64).unwrap_or(self.fps)
    }
}

struct Running {
    session: Arc<record::Session>,
    stop: StopHandle,
    generation: u64,
}

struct State {
    devs: Vec<CaptureDevice>,
    test_source: bool,
    resolutions: Vec<(i32, i32)>,
    rates: Vec<RateChoice>,
    mics: Vec<String>,
    output_dir: PathBuf,
    /// Bitrate the app last filled in by itself; if the user changed it, keep theirs.
    auto_bitrate: i32,
    running: Option<Running>,
    /// Start a new session once the running one has let go of the card.
    pending: bool,
    /// Record as soon as the next session starts (continuing after a dropout).
    record_on_start: bool,
    /// The mic menu entry the running session opened.
    session_mic: i32,
    generation: u64,
    /// Recordings still being finished (mixed, converted) in the background.
    finishing: u32,
    /// When the recording last restarted itself after a signal drop.
    auto_restarts: Vec<std::time::Instant>,
    /// Session threads, waited for on exit so no recording is cut off.
    workers: Vec<std::thread::JoinHandle<()>>,
    /// Frame rate to pick next time the list is filled (restoring saved settings).
    wanted_rate: Option<f64>,

}

pub fn run(devs: Vec<CaptureDevice>, test_source: bool) -> Result<(), String> {
    let ui = AppWindow::new().map_err(|e| format!("could not open the window: {e}"))?;
    ui.set_version(env!("CARGO_PKG_VERSION").into());

    let output_dir = std::env::var_os("XDG_VIDEOS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Lucentia");
    ui.set_output_folder(output_dir.display().to_string().into());

    let mut device_names: Vec<SharedString> = devs
        .iter()
        .map(|d| {
            let tag = if d.is_elgato { "" } else { " (not Elgato)" };
            format!("{}{tag}", d.name).into()
        })
        .collect();
    if test_source {
        device_names = vec!["Test pattern".into()];
    }
    ui.set_devices(strings(device_names));

    let mut mics = vec!["None".to_string()];
    mics.extend(
        devices::list_mics()
            .iter()
            .map(|d| d.display_name().to_string()),
    );
    if test_source {
        // With the test pattern, the mic is a test tone (any name works).
        mics.push("Test tone".to_string());
    }
    ui.set_codecs(strings(codec_labels()));
    ui.set_mics(strings(mics.iter().map(SharedString::from).collect()));

    let state = Rc::new(RefCell::new(State {
        devs,
        test_source,
        resolutions: Vec::new(),
        rates: Vec::new(),
        mics,
        output_dir,
        auto_bitrate: 0,
        running: None,
        pending: false,
        record_on_start: false,
        session_mic: 0,
        generation: 0,
        finishing: 0,
        auto_restarts: Vec::new(),
        workers: Vec::new(),
        wanted_rate: None,
    }));

    STATE.with(|s| *s.borrow_mut() = Some(state.clone()));

    // Appearance: follow the system unless the user picked Light or Dark.
    ui.set_theme_index(match load_setting("theme").as_deref() {
        Some("light") => 1,
        Some("dark") => 2,
        _ => 0,
    });
    ui.invoke_apply_theme();
    ui.set_ask_save(load_setting("ask_save").as_deref() != Some("false"));
    ui.on_ask_save_changed(|on| {
        if let Err(e) = save_setting("ask_save", if on { "true" } else { "false" }) {
            log::warn(&format!("could not save the setting: {e}"));
        }
    });
    ui.set_settings_open(load_setting("settings_open").as_deref() == Some("true"));
    {
        let weak = ui.as_weak();
        ui.on_settings_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let open = if ui.get_settings_open() { "true" } else { "false" };
            if let Err(e) = save_setting("settings_open", open) {
                log::warn(&format!("could not save the setting: {e}"));
            }
        });
    }
    ui.on_theme_changed(|i| {
        let name = match i {
            1 => "light",
            2 => "dark",
            _ => "auto",
        };
        if let Err(e) = save_setting("theme", name) {
            log::warn(&format!("could not save the theme setting: {e}"));
        }
    });

    restore_settings(&ui, &state);

    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_device_changed(move || {
            let ui = w.unwrap();
            fill_resolutions(&ui, &s);
            fill_rates(&ui, &s);
            refresh_estimate(&ui, &s);
            restart(&ui, &s);
            remember(&ui, &s.borrow());
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_mode_changed(move || {
            let ui = w.unwrap();
            fill_rates(&ui, &s);
            refresh_estimate(&ui, &s);
            restart(&ui, &s);
            remember(&ui, &s.borrow());
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_settings_changed(move || {
            let ui = w.unwrap();
            refresh_estimate(&ui, &s);
            // The mic is opened by the session, so picking a different one restarts
            // the preview (only while not recording; the menu is locked then).
            let mic = ui.get_mic_index();
            if s.borrow().session_mic != mic && !ui.get_recording() {
                restart(&ui, &s);
            }
            remember(&ui, &s.borrow());
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        // Turning game audio on or off adds or removes it from the preview.
        ui.on_game_audio_toggled(move || {
            let ui = w.unwrap();
            refresh_estimate(&ui, &s);
            if !ui.get_recording() {
                restart(&ui, &s);
            }
            remember(&ui, &s.borrow());
        });
    }
    {
        // Volume and mute change live, without restarting anything.
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_audio_levels_changed(move || {
            let ui = w.unwrap();
            if let Some(r) = &s.borrow().running {
                r.session
                    .controls
                    .set_game_volume(ui.get_game_volume().round() as f64);
                r.session
                    .controls
                    .set_mic_volume(ui.get_mic_volume().round() as f64);
                r.session.controls.set_listen_muted(ui.get_listen_muted());
                r.session.controls.set_mic_muted(ui.get_mic_muted());
            }
            remember(&ui, &s.borrow());
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_record_clicked(move || {
            let ui = w.unwrap();
            if ui.get_recording() {
                stop_recording(&ui, &s);
            } else {
                start_recording(&ui, &s);
            }
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_choose_folder(move || {
            let ui = w.unwrap();
            let start = s.borrow().output_dir.clone();
            let _ = std::fs::create_dir_all(&start);
            if let Some(dir) = rfd::FileDialog::new().set_directory(&start).pick_folder() {
                ui.set_output_folder(dir.display().to_string().into());
                s.borrow_mut().output_dir = dir;
                refresh_estimate(&ui, &s);
                remember(&ui, &s.borrow());
            }
        });
    }
    // Closing the main window quits, even with the pop-out window still open.
    ui.window().on_close_requested(|| {
        let _ = slint::quit_event_loop();
        slint::CloseRequestResponse::HideWindow
    });
    {
        let (w, s) = (ui.as_weak(), state.clone());
        ui.on_popout_clicked(move || {
            let ui = w.unwrap();
            toggle_popout(&ui, &s);
        });
    }
    {
        let (w, s) = (ui.as_weak(), state.clone());
        // The echo canceller is part of the mic's path, so the preview restarts.
        ui.on_echo_cancel_toggled(move || {
            let ui = w.unwrap();
            if !ui.get_recording() {
                restart(&ui, &s);
            }
            remember(&ui, &s.borrow());
        });
    }
    {
        let s = state.clone();
        ui.on_open_folder(move || {
            let dir = s.borrow().output_dir.clone();
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = std::process::Command::new("xdg-open").arg(&dir).spawn() {
                log::warn(&format!("could not open {}: {e}", dir.display()));
            }
        });
    }

    // Record hotkey: the desktop's global shortcut, and `lucentia toggle-record`.
    {
        let toggle = {
            let w = ui.as_weak();
            move || {
                let _ = w.upgrade_in_event_loop(|ui| {
                    if !ui.get_busy() {
                        ui.invoke_record_clicked();
                    }
                });
            }
        };
        crate::hotkey::listen(toggle.clone());
        let w = ui.as_weak();
        crate::hotkey::register(
            move |key| {
                let text = match key {
                    Some(k) => format!("Start or stop recording from anywhere with {k}."),
                    None => "Your desktop has no global shortcuts portal. Bind the command \
                             `lucentia toggle-record` to a key in its keyboard settings."
                        .to_string(),
                };
                let _ = w.upgrade_in_event_loop(move |ui| ui.set_hotkey_text(text.into()));
            },
            toggle,
        );
    }

    // Level meters: read the running session's audio peaks 20 times a second.
    let meters = slint::Timer::default();
    {
        let (w, s) = (ui.as_weak(), state.clone());
        meters.start(slint::TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = w.upgrade() else { return };
            let (game, mic) = s
                .borrow()
                .running
                .as_ref()
                .map(|r| r.session.levels())
                .unwrap_or((-100.0, -100.0));
            // -60 dB (quiet) to 0 dB (full scale).
            let fill = |db: f32| ((db + 60.0) / 60.0).clamp(0.0, 1.0);
            ui.set_game_level(fill(game));
            ui.set_mic_level(fill(mic));
        });
    }

    // Say up front if a plugin or the GPU encoder is missing (e.g. no VA-API driver).
    // Before the preview starts, so a preview error isn't covered up by this note.
    let report = crate::check::run();
    for n in &report.notes {
        log::warn(n);
    }
    if let Some((text, error)) = crate::check::summary(&report) {
        show(&ui, &text, error);
    }
    restart(&ui, &state);
    ui.run().map_err(|e| e.to_string())?;
    crate::hotkey::remove_socket();

    // Window closed: stop whatever is running and wait for every session to finish
    // its file, so a recording is never cut off.
    if let Some(r) = state.borrow_mut().running.take() {
        // The session's end of stream also completes an attached recording's file.
        r.stop.stop();
        let session = r.session.clone();
        let opts = options(&ui, &state.borrow(), true);
        state.borrow_mut().workers.push(std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            if let (Some(rec), Some(opts)) = (session.stop_recording(), opts) {
                let _ = session.wait_recording(&rec, Duration::from_secs(10));
                let _ = record::finish_recording(rec, &opts, &|_| {});
            }
        }));
    }
    let workers = std::mem::take(&mut state.borrow_mut().workers);
    if !workers.is_empty() {
        log::debug("waiting for recordings to finish before exiting");
    }
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn strings(v: Vec<SharedString>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(v))
}

/// Modes for the selected device (a fixed set for the test pattern).
fn device_modes(ui: &AppWindow, s: &State) -> Vec<Mode> {
    if s.test_source {
        let m = |w, h, fps: i32, f: &str| Mode {
            kind: Kind::Lossless,
            width: w,
            height: h,
            fps_num: fps,
            fps_den: 1,
            format: f.into(),
        };
        return vec![
            m(1920, 1080, 60, "NV12"),
            m(1920, 1080, 30, "NV12"),
            m(1920, 1080, 30, "P010_10LE"),
            m(1280, 720, 120, "NV12"),
            m(1280, 720, 60, "NV12"),
        ];
    }
    s.devs
        .get(ui.get_device_index().max(0) as usize)
        .map(|d| d.modes.clone())
        .unwrap_or_default()
}

fn fill_resolutions(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let modes = device_modes(ui, &state.borrow());
    let mut res: Vec<(i32, i32)> = modes.iter().map(|m| (m.width, m.height)).collect();
    res.sort_by_key(|(w, h)| std::cmp::Reverse((*w as i64) * (*h as i64)));
    res.dedup();
    let labels: Vec<SharedString> = res.iter().map(|&(w, h)| res_label(w, h).into()).collect();
    // Default to 1080p when the device has it.
    let default = res.iter().position(|&r| r == (1920, 1080)).unwrap_or(0);
    ui.set_resolutions(strings(labels));
    ui.set_resolution_index(default as i32);
    state.borrow_mut().resolutions = res;
}

fn res_label(w: i32, h: i32) -> String {
    let name = match (w, h) {
        (3840, 2160) => " (4K)",
        (2560, 1440) => " (1440p)",
        (3440, 1440) => " (ultrawide)",
        (2560, 1080) => " (ultrawide)",
        (1920, 1080) => " (1080p)",
        (1280, 720) => " (720p)",
        _ => "",
    };
    format!("{w} x {h}{name}")
}

fn fill_rates(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let modes = device_modes(ui, &state.borrow());
    let Some(&(w, h)) = state
        .borrow()
        .resolutions
        .get(ui.get_resolution_index().max(0) as usize)
    else {
        return;
    };
    let previous = state
        .borrow()
        .rates
        .get(ui.get_rate_index().max(0) as usize)
        .map(|r| r.shown());
    let previous = state.borrow_mut().wanted_rate.take().or(previous);
    let mut rates: Vec<RateChoice> = Vec::new();
    for m in modes.iter().filter(|m| m.width == w && m.height == h) {
        let fps = m.fps();
        let entry = match rates.iter_mut().find(|r| (r.fps - fps).abs() < 0.01) {
            Some(e) => e,
            None => {
                rates.push(RateChoice {
                    fps,
                    lossless: false,
                    hdr: false,
                    record_fps: None,
                });
                rates.last_mut().unwrap()
            }
        };
        if m.kind == Kind::Lossless {
            entry.lossless = true;
            entry.hdr |= m.ten_bit();
        }
    }
    // 40 fps (Steam Deck / handheld style 40 Hz modes): capture at 120 and keep every
    // third frame, which lines up exactly. Only offered where the card can do 120.
    let has_40 = rates.iter().any(|r| (r.fps - 40.0).abs() < 0.01);
    if let Some(r120) = rates.iter().find(|r| (r.fps - 120.0).abs() < 0.01) {
        if !has_40 {
            rates.push(RateChoice {
                record_fps: Some(40),
                ..r120.clone()
            });
        }
    }
    rates.sort_by(|a, b| b.shown().total_cmp(&a.shown()));
    let labels: Vec<SharedString> = rates
        .iter()
        .map(|r| {
            let fps = preview::fmt_fps(r.fps);
            if let Some(f) = r.record_fps {
                format!("{f} fps (from {fps} Hz)").into()
            } else if r.lossless {
                format!("{fps} fps").into()
            } else {
                format!("{fps} fps (MJPEG)").into()
            }
        })
        .collect();
    // Keep the same frame rate when switching resolution, else prefer 60.
    let index = previous
        .and_then(|p| rates.iter().position(|r| (r.shown() - p).abs() < 0.01))
        .or_else(|| rates.iter().position(|r| (r.fps - 60.0).abs() < 0.01))
        .unwrap_or(0);
    ui.set_rates(strings(labels));
    ui.set_rate_index(index as i32);
    let choice = rates.get(index).cloned();
    state.borrow_mut().rates = rates;

    if let Some(c) = choice {
        ui.set_hdr_available(c.hdr);
        if !c.hdr {
            ui.set_hdr(false);
        }
        let note = match (c.lossless, ui.get_hdr()) {
            _ if c.record_fps.is_some() => format!(
                "Captures at {} Hz and keeps every third frame, so set your console or PC to output 120 Hz. The preview shows all 120.",
                preview::fmt_fps(c.fps)
            ),
            (false, _) => "Compressed on the card (MJPEG): this rate is beyond what the USB link can carry uncompressed, so expect some quality loss.".to_string(),
            (true, true) => "Lossless 10-bit HDR from the card.".to_string(),
            (true, false) if c.hdr => "Lossless from the card. 10-bit HDR is available at this rate.".to_string(),
            (true, false) => "Lossless from the card. 10-bit HDR isn't available at this rate.".to_string(),
        };
        ui.set_mode_note(note.into());
    }
}

/// Open or close the pop-out video window. While it's open, frames are sent at up
/// to 1080p so a Discord share isn't blurry; back to 720p when it closes.
fn toggle_popout(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let open = POPOUT.with(|p| p.borrow().is_some());
    if open {
        close_popout(ui, state);
        return;
    }
    let win = match VideoWindow::new() {
        Ok(w) => w,
        Err(e) => {
            show(ui, &format!("could not open the video window: {e}"), true);
            return;
        }
    };
    {
        let w = win.as_weak();
        win.on_toggle_fullscreen(move || {
            if let Some(w) = w.upgrade() {
                let full = w.window().is_fullscreen();
                w.window().set_fullscreen(!full);
            }
        });
    }
    {
        let (u, s) = (ui.as_weak(), state.clone());
        win.window().on_close_requested(move || {
            if let Some(ui) = u.upgrade() {
                close_popout(&ui, &s);
            }
            slint::CloseRequestResponse::HideWindow
        });
    }
    if let Err(e) = win.show() {
        show(ui, &format!("could not open the video window: {e}"), true);
        return;
    }
    POPOUT.with(|p| *p.borrow_mut() = Some(win));
    ui.set_popout_open(true);
    if let Some(r) = &state.borrow().running {
        r.session.set_preview_width(1920);
    }
}

fn close_popout(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    if let Some(w) = POPOUT.with(|p| p.borrow_mut().take()) {
        let _ = w.hide();
    }
    ui.set_popout_open(false);
    if let Some(r) = &state.borrow().running {
        r.session.set_preview_width(1280);
    }
}

/// Order of the Codec menu.
const CODECS: [Codec; 4] = [Codec::Hevc, Codec::Av1, Codec::H264, Codec::Raw];

fn codec_from_index(i: i32) -> Codec {
    CODECS.get(i.max(0) as usize).copied().unwrap_or(Codec::Hevc)
}

/// Codec menu labels, marking the ones this machine can't do on the GPU.
fn codec_labels() -> Vec<SharedString> {
    use crate::check::{codec_support, Support};
    CODECS
        .iter()
        .map(|&c| {
            let name = if c == Codec::Raw { "Raw (uncompressed)" } else { c.name() };
            match codec_support(c, false) {
                Support::Gpu => name.to_string(),
                Support::Cpu => format!("{name} (CPU)"),
                Support::None => format!("{name} (not available)"),
            }
            .into()
        })
        .collect()
}

/// The record options matching what's on screen.
fn options(ui: &AppWindow, s: &State, record_on: bool) -> Option<record::Options> {
    let &(w, h) = s
        .resolutions
        .get(ui.get_resolution_index().max(0) as usize)?;
    let rate = s.rates.get(ui.get_rate_index().max(0) as usize)?;
    let hdr = ui.get_hdr() && rate.hdr;
    let codec = codec_from_index(ui.get_codec_index());
    let kbps = (ui.get_bitrate().max(1) as u32) * 1000;
    let rate_control = match ui.get_rate_mode_index() {
        1 => RateControl::Vbr {
            kbps,
            max_kbps: kbps * 3 / 2,
        },
        2 => RateControl::Quality {
            q: ui.get_quality().clamp(0, 51) as u32,
        },
        _ => RateControl::Cbr { kbps },
    };
    let mic = match ui.get_mic_index() {
        i if i > 0 => s.mics.get(i as usize).cloned(),
        _ => None,
    };
    Some(record::Options {
        capture: preview::Options {
            mode: Some((w, h, rate.fps)),
            hdr,
            mjpeg: !rate.lossless,
            hdr_signal: ui.get_hdr_signal() && !hdr,
            audio: ui.get_game_audio(),
        },
        codec: Some(codec),
        rate: Some(rate_control),
        keyframe_secs: 2.0,
        analysis: ui.get_analysis(),
        record_fps: rate.record_fps,
        audio_codec: AudioCodec::Aac,
        audio_kbps: 320,
        mic,
        mic_offset_ms: ui.get_mic_offset() as i64,
        mix_track: ui.get_mix_track(),
        game_volume: ui.get_game_volume().round() as f64,
        mic_volume: ui.get_mic_volume().round() as f64,
        container: if ui.get_format_index() == 1 {
            Container::Mp4
        } else {
            Container::Mkv
        },
        output: Some(
            s.output_dir
                .join(format!("Lucentia {}.mkv", record::local_timestamp())),
        ),
        duration: None,
        preview: false,
        test_source: s.test_source,
        record: record_on,
        frames: None,
        listen_muted: ui.get_listen_muted(),
        mic_muted: ui.get_mic_muted(),
        echo_cancel: ui.get_echo_cancel(),
    })
}

fn card<'a>(ui: &AppWindow, s: &'a State) -> Option<&'a CaptureDevice> {
    if s.test_source {
        None
    } else {
        s.devs.get(ui.get_device_index().max(0) as usize)
    }
}

/// Recompute the size estimate and drive space for the current settings, and fill in
/// a sensible bitrate for the mode unless the user picked their own.
fn refresh_estimate(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    // Say why a codec isn't on the GPU (or isn't available at all).
    let codec = codec_from_index(ui.get_codec_index());
    let (note, error) = crate::check::codec_note(codec, ui.get_hdr())
        .unwrap_or_default();
    ui.set_codec_note(note.into());
    ui.set_codec_note_error(error);
    {
        let s = state.borrow();
        if let Some(&(w, h)) = s.resolutions.get(ui.get_resolution_index().max(0) as usize) {
            if let Some(r) = s.rates.get(ui.get_rate_index().max(0) as usize) {
                let mode = Mode {
                    kind: Kind::Lossless,
                    width: w,
                    height: h,
                    fps_num: (r.shown() * 1000.0).round() as i32,
                    fps_den: 1000,
                    format: String::new(),
                };
                let codec = codec_from_index(ui.get_codec_index());
                let auto = (record::default_kbps(&mode, codec) as f64 / 1000.0).round() as i32;
                let user_changed = ui.get_bitrate() != s.auto_bitrate && s.auto_bitrate != 0;
                drop(s);
                if !user_changed {
                    ui.set_bitrate(auto);
                }
                state.borrow_mut().auto_bitrate = if user_changed { -1 } else { auto };
            }
        }
    }
    let s = state.borrow();
    let Some(opts) = options(ui, &s, true) else {
        ui.set_estimate("".into());
        return;
    };
    match record::plan(card(ui, &s), &opts) {
        Ok(plan) => {
            let mut text = format!("Size: {}", estimate::describe(plan.est));
            if let Some(free) = estimate::free_space(&s.output_dir)
                .or_else(|| estimate::free_space(s.output_dir.parent().unwrap_or(&s.output_dir)))
            {
                let secs = free as f64 / plan.est.high.max(1.0);
                text.push_str(&format!(
                    "\nDrive: {} free, about {} at this rate.",
                    estimate::human_bytes(free as f64),
                    estimate::human_duration(secs)
                ));
            }
            ui.set_estimate(text.into());
            if !ui.get_recording() {
                show(ui, &plan.warnings.join(" "), false);
            }
        }
        Err(e) => {
            ui.set_estimate("".into());
            show(ui, &e, true);
        }
    }
}

fn show(ui: &AppWindow, message: &str, error: bool) {
    ui.set_message(message.into());
    ui.set_message_is_error(error);
    ui.set_message_is_success(false);
}

fn show_success(ui: &AppWindow, message: &str) {
    ui.set_message(message.into());
    ui.set_message_is_error(false);
    ui.set_message_is_success(true);
}

/// Start a new preview session (the card opens once and stays open), stopping the
/// current one first. Only for things that change the session's inputs: device, mode,
/// game audio on/off, mic choice.
fn restart(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    if let Some(r) = &state.borrow().running {
        r.stop.stop();
    }
    if state.borrow().running.is_some() {
        // Start it once the current session has let go of the card (see `released`).
        state.borrow_mut().pending = true;
        return;
    }
    start_session(ui, state);
}

fn start_session(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let Some(mut opts) = options(ui, &state.borrow(), false) else {
        ui.set_has_preview(false);
        ui.set_preview_message("This device reported no usable modes.".into());
        return;
    };

    // Frames go straight to the window. Only one frame is in flight at a time: if the
    // window hasn't drawn the last one yet, newer frames are skipped, so the preview
    // can never fall behind.
    let weak = ui.as_weak();
    let in_flight = Arc::new(AtomicBool::new(false));
    opts.frames = Some(Arc::new(move |f: Frame| {
        if in_flight.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut buf = SharedPixelBuffer::<slint::Rgba8Pixel>::new(f.width, f.height);
        buf.make_mut_bytes().copy_from_slice(&f.rgba);
        let flag = in_flight.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let image = slint::Image::from_rgba8(buf);
            // The pop-out window shows the same frame (no extra copy).
            POPOUT.with(|p| {
                if let Some(w) = &*p.borrow() {
                    w.set_frame(image.clone());
                    w.set_has_frame(true);
                }
            });
            ui.set_preview(image);
            ui.set_has_preview(true);
            flag.store(false, Ordering::Release);
        });
    }));

    let generation = {
        let mut s = state.borrow_mut();
        s.generation += 1;
        s.session_mic = ui.get_mic_index();
        s.generation
    };

    let session = {
        let s = state.borrow();
        let card = card(ui, &s);
        record::plan(card, &opts).and_then(|plan| record::start(card, &opts, plan))
    };
    let session = match session {
        Ok(sess) => Arc::new(sess),
        Err(e) => {
            ui.set_has_preview(false);
            ui.set_preview_message("No preview".into());
            log::error(&format!("preview: {e}"));
            show(ui, &e, true);
            return;
        }
    };

    // Keep the preview box the same shape as the video, so it fills it edge to edge.
    let mode = &session.plan.mode;
    ui.set_preview_aspect(mode.width as f32 / mode.height.max(1) as f32);

    state.borrow_mut().running = Some(Running {
        stop: session.stop_handle(),
        session: session.clone(),
        generation,
    });
    if POPOUT.with(|p| p.borrow().is_some()) {
        session.set_preview_width(1920);
    }

    let weak = ui.as_weak();
    let worker = std::thread::spawn(move || {
        let stats_weak = weak.clone();
        let watched = session.watch(None, Duration::from_secs(1), |st| {
            let st = st.clone();
            let _ = stats_weak.upgrade_in_event_loop(move |ui| show_stats(&ui, &st));
        });
        // An error mid-recording: take what was recorded so far.
        let interrupted = session.abandon_recording();
        session.release();
        let error = watched.err();
        let had_recording = interrupted.is_some();
        let _ =
            weak.upgrade_in_event_loop(move |ui| released(&ui, generation, had_recording, error));
        if let Some(rec) = interrupted {
            let _ = session.wait_recording(&rec, Duration::from_secs(1));
            let (tx, rx) = mpsc::channel();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let opts = STATE.with(|s| {
                    s.borrow()
                        .as_ref()
                        .and_then(|st| options(&ui, &st.borrow(), true))
                });
                let _ = tx.send(opts);
            });
            if let Ok(Some(opts)) = rx.recv() {
                let finished = finish_with_progress(&weak, rec, &opts);
                let _ =
                    weak.upgrade_in_event_loop(move |ui| finished_recording(&ui, Some(finished)));
            }
        }
    });
    let mut s = state.borrow_mut();
    s.workers.retain(|w| !w.is_finished());
    s.workers.push(worker);
    drop(s);

    if std::mem::take(&mut state.borrow_mut().record_on_start) {
        start_recording(ui, state);
    }
}

/// Attach a recording to the running session. The capture isn't touched.
fn start_recording(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let Some(session) = state.borrow().running.as_ref().map(|r| r.session.clone()) else {
        show(ui, "Nothing to record: the capture isn't running.", true);
        return;
    };
    let result = {
        let s = state.borrow();
        match options(ui, &s, true) {
            Some(opts) => record::plan(card(ui, &s), &opts)
                .and_then(|plan| session.start_recording(&opts, plan.clone()).map(|_| plan)),
            None => Err("no usable mode selected".to_string()),
        }
    };
    match result {
        Ok(plan) => {
            // Just the file name: the folder is under Settings > Output.
            let name = plan
                .final_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| plan.final_path.display().to_string());
            let mut text = vec![format!("Recording to {name}")];
            text.extend(plan.warnings);
            show(ui, &text.join(" "), false);
            ui.set_recording(true);
            ui.set_lost_text("0".into());
            ui.set_lost_warning(false);
            ui.set_size_text("–".into());
            ui.set_bitrate_text("–".into());
            ui.set_drive_text("–".into());
            ui.set_time_text("0:00".into());
        }
        Err(e) => show(ui, &e, true),
    }
}

/// Detach the recording and finish its file in the background. The preview and the
/// capture keep running untouched.
fn stop_recording(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let Some(session) = state.borrow().running.as_ref().map(|r| r.session.clone()) else {
        return;
    };
    let Some(rec) = session.stop_recording() else {
        ui.set_recording(false);
        return;
    };
    let Some(opts) = options(ui, &state.borrow(), true) else {
        return;
    };
    ui.set_recording(false);
    {
        let mut s = state.borrow_mut();
        s.finishing += 1;
    }
    ui.set_busy(true);
    if record::needs_stitching(&rec.plan, &opts) {
        ui.set_finish_text("Stitching…".into());
        show(ui, "Stitching the recording together…", false);
    } else {
        ui.set_finish_text("Finishing…".into());
        show(ui, "Finishing the recording…", false);
    }

    let weak = ui.as_weak();
    let worker = std::thread::spawn(move || {
        if !session.wait_recording(&rec, Duration::from_secs(60)) {
            log::warn("the recording took too long to close; finishing what was written");
        }
        let finished = finish_with_progress(&weak, rec, &opts);
        let _ = weak.upgrade_in_event_loop(move |ui| finished_recording(&ui, Some(finished)));
    });
    let mut s = state.borrow_mut();
    s.workers.retain(|w| !w.is_finished());
    s.workers.push(worker);
}

/// Finish a recording's file, showing "Stitching… 42% · about 10s left" in the
/// message bar while the Game + Mic mix or the MP4 conversion runs.
fn finish_with_progress(
    weak: &slint::Weak<AppWindow>,
    rec: record::StoppedRecording,
    opts: &record::Options,
) -> record::Finished {
    if !record::needs_stitching(&rec.plan, opts) {
        return record::finish_recording(rec, opts, &|_| {});
    }
    let _ = weak.upgrade_in_event_loop(|ui| {
        ui.set_finish_text("Stitching…".into());
        show(&ui, "Stitching the recording together…", false);
    });
    let started = std::time::Instant::now();
    let last = std::cell::Cell::new(String::new());
    record::finish_recording(rec, opts, &|done| {
        let text = format!(
            "Stitching the recording together… {}",
            estimate::describe_progress(started, done)
        );
        if text != last.replace(text.clone()) {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_finish_text(format!("Stitching… {}%", (done * 100.0).floor().min(99.0)).into());
                show(&ui, &text, false);
            });
        }
    })
}

/// Settings > Debug: what's happening, and what to do about lost frames.
fn show_diagnostics(ui: &AppWindow, st: &record::Stats) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    let s = state.borrow();
    let Some(running) = &s.running else { return };
    let plan = &running.session.plan;
    let mode = &plan.mode;
    let device = card(ui, &s);
    let link = device
        .and_then(|d| d.product_id.as_deref())
        .and_then(devices::link_speed_note);

    let mut lines = vec![
        format!(
            "Capture: {} at {}x{}@{} {}",
            device.map(|d| d.name.as_str()).unwrap_or("test pattern"),
            mode.width,
            mode.height,
            preview::fmt_fps(mode.fps()),
            if mode.kind == Kind::Mjpeg { "MJPEG" } else { mode.format.as_str() }
        ),
        format!("USB: {}", link.unwrap_or("full speed")),
        format!(
            "GPU: {}",
            crate::check::gpu_name().unwrap_or_else(|| "unknown".into())
        ),
    ];
    if let Some(enc) = &st.encoder {
        let kind = if enc.starts_with("va") { "GPU" } else { "CPU" };
        lines.push(format!(
            "Encoder: {enc} ({kind}), {:.1} s waiting",
            st.encoder_backlog
        ));
    }
    lines.push(format!(
        "Frames: {} lost in {} gap(s), longest {:.0} ms; {} damaged frame(s) dropped",
        st.lost_total, st.gaps, st.worst_gap_ms, st.damaged
    ));
    lines.push(format!(
        "Preview: {:.1} fps of {}; CPU: {:.0}%",
        st.fps,
        preview::fmt_fps(mode.fps()),
        st.cpu_pct
    ));

    let mut advice = Vec::new();
    if st.damaged > 0 {
        advice.push(
            "Damaged frames: the card got a broken HDMI signal. A few when a game starts or \
             changes modes are normal; a steady stream means try another HDMI cable."
                .to_string(),
        );
    }
    if st.gaps > 0 && st.worst_gap_ms > 300.0 {
        advice.push(format!(
            "The HDMI signal dropped out for up to {:.0} ms. That happens when the source \
             switches resolution, refresh rate or HDR (often when a game starts). Set the \
             console or PC to a fixed output mode to avoid it.",
            st.worst_gap_ms
        ));
    } else if st.gaps > 0 {
        advice.push(match link {
            Some(l) => format!("Frames arrived late over USB: the card is {l}."),
            None => "Frames arrived late over USB. Plug the card straight into a 10 Gbps \
                     USB-C port, not a hub or dock, and close other heavy USB devices."
                .to_string(),
        });
    }
    if st.encoder_backlog > 1.0 {
        let enc = st.encoder.clone().unwrap_or_default();
        advice.push(if enc.starts_with("va") {
            format!(
                "The GPU encoder is falling behind ({:.1} s of video waiting). Lower the \
                 bitrate, resolution or frame rate.",
                st.encoder_backlog
            )
        } else {
            format!(
                "The CPU encoder ({enc}) can't keep up ({:.1} s of video waiting). Pick a \
                 codec your GPU encodes, or lower the resolution or frame rate.",
                st.encoder_backlog
            )
        });
    }
    if st.cpu_pct > 85.0 {
        advice.push(format!(
            "Lucentia is using {:.0}% of the CPU. Closing other apps or using the GPU encoder \
             will help.",
            st.cpu_pct
        ));
    }
    if st.lost_total == 0 && st.fps > 0.0 && st.fps < mode.fps() * 0.85 {
        advice.push(
            "The preview shows fewer frames than the capture. That's only the window; \
             recordings get every frame."
                .to_string(),
        );
    }
    if advice.is_empty() {
        advice.push("No problems found.".to_string());
    }
    ui.set_diagnostics(lines.join("\n").into());
    ui.set_advice(advice.join("\n\n").into());
}

fn show_stats(ui: &AppWindow, st: &record::Stats) {
    show_diagnostics(ui, st);
    ui.set_fps_text(format!("{:.2}", st.fps).into());
    ui.set_lost_text(st.lost.to_string().into());
    ui.set_lost_warning(st.lost > 0);
    if st.recording {
        let t = st.elapsed as u64;
        let time = if t >= 3600 {
            format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
        } else {
            format!("{}:{:02}", t / 60, t % 60)
        };
        ui.set_time_text(time.into());
        ui.set_size_text(estimate::human_bytes(st.size as f64).into());
        ui.set_bitrate_text(format!("{:.1} Mbit/s", st.mbps).into());
        ui.set_drive_text(
            st.drive_left
                .map(|s| estimate::human_duration(s))
                .unwrap_or_else(|| "–".into())
                .into(),
        );
    }
}

/// A session has stopped and let go of the card. Start the next one right away:
/// after a settings change, or (after a dropout mid-recording) to keep recording.
fn released(ui: &AppWindow, generation: u64, was_recording: bool, error: Option<String>) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    let was_current = state
        .borrow()
        .running
        .as_ref()
        .is_some_and(|r| r.generation == generation);
    if !was_current {
        return;
    }
    state.borrow_mut().running = None;
    ui.set_recording(false);

    let mut start = std::mem::take(&mut state.borrow_mut().pending);
    if was_recording {
        state.borrow_mut().finishing += 1;
        ui.set_busy(true);
    }
    if let Some(e) = &error {
        if was_recording && may_auto_continue(&state) {
            // The capture dropped out mid-recording. Keep going in a new file rather
            // than stopping.
            show(
                ui,
                &format!(
                    "The capture dropped out ({e}). The recording so far is saved, and recording continues in a new file."
                ),
                true,
            );
            state.borrow_mut().record_on_start = true;
            start = true;
        } else {
            show(ui, e, true);
        }
    }
    if start {
        start_session(ui, &state);
    } else if error.is_some() {
        ui.set_has_preview(false);
        ui.set_preview_message("Preview stopped. Change a setting to try again.".into());
    }
}

/// Allow at most 3 automatic restarts per minute, so a card that keeps failing
/// doesn't loop forever.
fn may_auto_continue(state: &Rc<RefCell<State>>) -> bool {
    let mut s = state.borrow_mut();
    let now = std::time::Instant::now();
    s.auto_restarts
        .retain(|t| now.duration_since(*t) < Duration::from_secs(60));
    if s.auto_restarts.len() >= 3 {
        return false;
    }
    s.auto_restarts.push(now);
    true
}

/// A recording's file is finished (mixed, converted). Report it, then offer Save As.
fn finished_recording(ui: &AppWindow, finished: Option<record::Finished>) {
    let Some(state) = STATE.with(|s| s.borrow().clone()) else {
        return;
    };
    {
        let mut s = state.borrow_mut();
        s.finishing = s.finishing.saturating_sub(1);
        ui.set_busy(s.finishing > 0);
    }
    let Some(f) = finished else { return };
    let mut text = format!(
        "Saved {} ({}). ",
        f.path.display(),
        estimate::human_bytes(f.size as f64)
    );
    text.push_str(&if f.lost == 0 {
        "No frames lost.".to_string()
    } else {
        format!("{} frame(s) were lost.", f.lost)
    });
    if let Some(e) = &f.finish_error {
        text.push_str(&format!(
            " Finishing failed ({e}); the MKV with separate tracks is safe."
        ));
    }
    if f.lost > 0 || f.finish_error.is_some() {
        show(ui, &text, true);
    } else {
        show_success(ui, &text);
    }
    if ui.get_ask_save() {
        ask_where_to_save(ui, f.path);
    }
}

/// Ask where to save a finished recording, pre-filled with its folder and name. The
/// dialog runs on its own thread so the window and preview keep running while it's
/// open. Cancel (or no file dialog available, e.g. in Game Mode) leaves it in place.
fn ask_where_to_save(ui: &AppWindow, current: PathBuf) {
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let Some(ext) = current
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_string)
        else {
            return;
        };
        let (Some(dir), Some(name)) = (current.parent(), current.file_name()) else {
            return;
        };
        let Some(mut dest) = rfd::FileDialog::new()
            .set_title("Save recording")
            .set_directory(dir)
            .set_file_name(name.to_string_lossy())
            .add_filter(ext.to_uppercase(), &[ext.as_str()])
            .save_file()
        else {
            return;
        };
        // The file is what it is: keep its real extension even if the name changed.
        if dest.extension().and_then(|e| e.to_str()) != Some(ext.as_str()) {
            let name = format!(
                "{}.{ext}",
                dest.file_name().unwrap_or_default().to_string_lossy()
            );
            dest.set_file_name(name);
        }
        if dest == current {
            return;
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            show(
                &ui,
                &format!("Moving the recording to {}…", dest.display()),
                false,
            );
            move_recording(&ui, current, dest);
        });
    });
}

/// Move a recording in the background: instant on the same drive, a copy when it goes
/// to another drive (which can take a while for big files).
fn move_recording(ui: &AppWindow, from: PathBuf, to: PathBuf) {
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let result = std::fs::rename(&from, &to).or_else(|e| {
            // EXDEV: different drives, so copy and then remove the original.
            if e.raw_os_error() == Some(libc::EXDEV) {
                std::fs::copy(&from, &to).and_then(|_| std::fs::remove_file(&from))
            } else {
                Err(e)
            }
        });
        let _ = weak.upgrade_in_event_loop(move |ui| match result {
            Ok(()) => show_success(&ui, &format!("Saved {}", to.display())),
            Err(e) => show(
                &ui,
                &format!(
                    "Couldn't move the recording to {} ({e}). It's still at {}.",
                    to.display(),
                    from.display()
                ),
                true,
            ),
        });
    });
}

thread_local! {
    /// The window's state, reachable from callbacks that only get the window handle.
    static STATE: RefCell<Option<Rc<RefCell<State>>>> = const { RefCell::new(None) };
    /// The pop-out video window, when open.
    static POPOUT: RefCell<Option<VideoWindow>> = const { RefCell::new(None) };
}

// ---- Settings file -------------------------------------------------------------------
//
// A small key=value file at ~/.config/lucentia/settings.conf: the theme and whether
// to ask where to save each recording.

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lucentia").join("settings.conf"))
}

fn load_setting(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(settings_path()?).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
}

/// Put back the settings from last time: card, mode, encoder, audio, output. Anything
/// that no longer exists (a card or mic that's unplugged) falls back to the default.
fn restore_settings(ui: &AppWindow, state: &Rc<RefCell<State>>) {
    let get = |k: &str| load_setting(k);
    let int = |k: &str| get(k).and_then(|v| v.parse::<i32>().ok());
    let float = |k: &str| get(k).and_then(|v| v.parse::<f32>().ok());
    let flag = |k: &str| get(k).map(|v| v == "true");

    if let Some(name) = get("device") {
        let s = state.borrow();
        if let Some(i) = s.devs.iter().position(|d| d.name == name) {
            ui.set_device_index(i as i32);
        }
    }
    fill_resolutions(ui, state);
    if let Some((w, h)) = get("resolution")
        .and_then(|r| r.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
        .and_then(|(w, h)| Some((w.ok()?, h.ok()?)))
    {
        if let Some(i) = state.borrow().resolutions.iter().position(|&r| r == (w, h)) {
            ui.set_resolution_index(i as i32);
        }
    }
    if let Some(v) = flag("hdr") {
        ui.set_hdr(v);
    }
    if let Some(v) = flag("hdr_signal") {
        ui.set_hdr_signal(v);
    }
    state.borrow_mut().wanted_rate = get("fps").and_then(|v| v.parse().ok());
    fill_rates(ui, state);

    if let Some(v) = int("codec") {
        ui.set_codec_index(v.clamp(0, 3));
    }
    if let Some(v) = int("rate_mode") {
        ui.set_rate_mode_index(v.clamp(0, 2));
    }
    if let Some(v) = int("quality") {
        ui.set_quality(v.clamp(0, 51));
    }
    if let Some(v) = flag("analysis") {
        ui.set_analysis(v);
    }
    if let Some(v) = flag("game_audio") {
        ui.set_game_audio(v);
    }
    if let Some(v) = float("game_volume") {
        ui.set_game_volume(v.clamp(0.0, 200.0));
    }
    if let Some(name) = get("mic") {
        if let Some(i) = state.borrow().mics.iter().position(|m| *m == name) {
            ui.set_mic_index(i as i32);
        }
    }
    if let Some(v) = float("mic_volume") {
        ui.set_mic_volume(v.clamp(0.0, 200.0));
    }
    if let Some(v) = int("mic_offset") {
        ui.set_mic_offset(v);
    }
    if let Some(v) = flag("mix_track") {
        ui.set_mix_track(v);
    }
    if let Some(v) = flag("echo_cancel") {
        ui.set_echo_cancel(v);
    }
    if let Some(v) = int("format") {
        ui.set_format_index(v.clamp(0, 1));
    }
    if let Some(dir) = get("output_dir").map(PathBuf::from) {
        ui.set_output_folder(dir.display().to_string().into());
        state.borrow_mut().output_dir = dir;
    }
    refresh_estimate(ui, state);
    // A bitrate the user typed in wins over the automatic one for the mode.
    if let Some(v) = int("bitrate") {
        ui.set_bitrate(v.max(1));
        state.borrow_mut().auto_bitrate = -1;
        refresh_estimate(ui, state);
    }
}

/// Save what's on screen, so the next launch starts the same way.
fn remember(ui: &AppWindow, s: &State) {
    let b = |v: bool| if v { "true" } else { "false" }.to_string();
    let mut items: Vec<(&str, Option<String>)> = vec![
        ("hdr", Some(b(ui.get_hdr()))),
        ("hdr_signal", Some(b(ui.get_hdr_signal()))),
        ("codec", Some(ui.get_codec_index().to_string())),
        ("rate_mode", Some(ui.get_rate_mode_index().to_string())),
        ("quality", Some(ui.get_quality().to_string())),
        ("analysis", Some(b(ui.get_analysis()))),
        ("game_audio", Some(b(ui.get_game_audio()))),
        ("game_volume", Some(ui.get_game_volume().round().to_string())),
        ("mic_volume", Some(ui.get_mic_volume().round().to_string())),
        ("mic_offset", Some(ui.get_mic_offset().to_string())),
        ("mix_track", Some(b(ui.get_mix_track()))),
        ("echo_cancel", Some(b(ui.get_echo_cancel()))),
        ("format", Some(ui.get_format_index().to_string())),
        ("output_dir", Some(s.output_dir.display().to_string())),
        // Drop the old virtual camera setting from earlier versions.
        ("virtual_camera", None),
        // Only a bitrate the user picked; otherwise each mode gets its own default.
        (
            "bitrate",
            (s.auto_bitrate == -1).then(|| ui.get_bitrate().to_string()),
        ),
        (
            "mic",
            s.mics
                .get(ui.get_mic_index().max(0) as usize)
                .filter(|_| ui.get_mic_index() > 0)
                .cloned()
                .or(Some("None".into())),
        ),
    ];
    if !s.test_source {
        if let Some(d) = s.devs.get(ui.get_device_index().max(0) as usize) {
            items.push(("device", Some(d.name.clone())));
        }
    }
    if let Some(&(w, h)) = s.resolutions.get(ui.get_resolution_index().max(0) as usize) {
        items.push(("resolution", Some(format!("{w}x{h}"))));
    }
    if let Some(r) = s.rates.get(ui.get_rate_index().max(0) as usize) {
        items.push(("fps", Some(r.shown().to_string())));
    }
    if let Err(e) = save_settings(&items) {
        log::warn(&format!("could not save settings: {e}"));
    }
}

fn save_setting(key: &str, value: &str) -> std::io::Result<()> {
    save_settings(&[(key, Some(value.to_string()))])
}

/// Set (Some) or remove (None) several keys in one write.
fn save_settings(items: &[(&str, Option<String>)]) -> std::io::Result<()> {
    let path = settings_path().ok_or_else(|| std::io::Error::other("no home folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| {
            let k = l.split_once('=').map(|(k, _)| k.trim());
            !items.iter().any(|(key, _)| Some(*key) == k)
        })
        .map(String::from)
        .collect();
    for (key, value) in items {
        if let Some(v) = value {
            lines.push(format!("{key}={v}"));
        }
    }
    // Write a temp file and rename, so a crash mid-write can't leave half a file.
    let tmp = path.with_extension("conf.tmp");
    std::fs::write(&tmp, lines.join("\n") + "\n")?;
    std::fs::rename(&tmp, &path)
}
