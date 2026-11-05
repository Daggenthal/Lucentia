//! Finishing a recording after it stops: optional Game + Mic mix, optional MP4.
//!
//! Two stages, each a simple pipeline:
//!   1. mix:  recording's Game and Mic tracks -> decode -> mix -> encode -> temp .mka
//!   2. copy: recording's video + Game + Mic, plus the mix -> final MKV or MP4
//! Video and the original audio tracks are copied as-is, so this takes seconds and
//! loses no quality. The mix goes first among the audio tracks so players play it by
//! default. Mixing here, from finished files, avoids the clock drift that made mixing
//! two live devices choppy.
//!
//! Everything is written to temporary files and only renamed into place once
//! complete, so a failure never damages the original recording.

use gst::prelude::*;
use gstreamer as gst;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::encode::{self, AudioCodec};
use crate::log;
use crate::preview::make;

/// Names for the recording's audio tracks, in the order Lucentia writes them.
const TRACK_NAMES: [&str; 2] = ["Game", "Mic"];

/// Rewrite `mkv` into `out` (MP4 if `mp4`), adding a Game + Mic track when `mix` is
/// set (audio codec and kbit/s for it).
pub fn finalize(
    mkv: &Path,
    out: &Path,
    mp4: bool,
    mix: Option<(AudioCodec, u32)>,
) -> Result<(), String> {
    let tmp_out = sibling(out, "finishing");
    let tmp_mix = sibling(mkv, "mix.mka");

    let result = (|| {
        let mix_file = match mix {
            Some((codec, kbps)) => {
                if audio_tracks(mkv)? < 2 {
                    log::warn("no separate Game and Mic tracks to mix, skipping the mix");
                    None
                } else {
                    mix_audio(mkv, &tmp_mix, codec, kbps)?;
                    Some(tmp_mix.as_path())
                }
            }
            None => None,
        };
        copy_into(mkv, mix_file, &tmp_out, mp4)
    })();

    let _ = std::fs::remove_file(&tmp_mix);
    match result {
        Ok(()) => {
            std::fs::rename(&tmp_out, out)
                .map_err(|e| format!("could not move the finished file into place: {e}"))?;
            if out != mkv {
                let _ = std::fs::remove_file(mkv);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_out);
            Err(e)
        }
    }
}

/// clip.mkv + "finishing" -> clip.finishing.mkv; clip.mkv + "mix.mka" -> clip.mix.mka
fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("mkv");
    if suffix.contains('.') {
        p.with_extension(suffix)
    } else {
        p.with_extension(format!("{suffix}.{ext}"))
    }
}

/// How many audio tracks a recording has: runs the demuxer just far enough for it to
/// announce every track.
fn audio_tracks(mkv: &Path) -> Result<usize, String> {
    let pipeline = gst::Pipeline::new();
    let src = make("filesrc")?;
    src.set_property("location", mkv.to_string_lossy().to_string());
    let demux = make("matroskademux")?;
    pipeline
        .add_many([&src, &demux])
        .map_err(|e| e.to_string())?;
    src.link(&demux)
        .map_err(|_| "could not read the recording")?;
    let count = Arc::new(Mutex::new(0usize));
    {
        let count = count.clone();
        let pipeline_weak = pipeline.downgrade();
        demux.connect_pad_added(move |_, pad| {
            if pad.name().starts_with("audio") {
                *count.lock().unwrap() += 1;
            }
            // Give every track somewhere to go so the demuxer can keep reading.
            if let (Some(p), Ok(sink)) = (pipeline_weak.upgrade(), make("fakesink")) {
                let _ = p.add(&sink);
                let _ = sink.sync_state_with_parent();
                let _ = pad.link(&sink.static_pad("sink").unwrap());
            }
        });
    }
    let done = Arc::new(Mutex::new(false));
    {
        let done = done.clone();
        demux.connect_no_more_pads(move |_| *done.lock().unwrap() = true);
    }
    pipeline
        .set_state(gst::State::Paused)
        .map_err(|_| "could not open the recording".to_string())?;
    let bus = pipeline.bus().ok_or("no bus")?;
    // Wait until the demuxer has seen all tracks (or the pipeline settles).
    let _ = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(10),
        &[gst::MessageType::AsyncDone, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    let n = *count.lock().unwrap();
    log::debug(&format!(
        "recording has {n} audio track(s) (all tracks seen: {})",
        *done.lock().unwrap()
    ));
    Ok(n)
}

/// Stage 1: decode the first two audio tracks, mix them, encode into `dest` (.mka).
fn mix_audio(mkv: &Path, dest: &Path, codec: AudioCodec, kbps: u32) -> Result<(), String> {
    let quote = |p: &Path| p.to_string_lossy().replace('"', "\\\"");
    let desc = format!(
        "filesrc location=\"{src}\" ! matroskademux name=d \
         d.video_0 ! queue ! fakesink sync=false \
         d.audio_0 ! queue ! decodebin ! audioconvert ! audioresample ! audiomixer name=m \
         d.audio_1 ! queue ! decodebin ! audioconvert ! audioresample ! m. \
         m. ! audioconvert ! audio/x-raw,rate=48000,channels=2 ! identity name=mixout \
         matroskamux name=mmux ! filesink location=\"{dest}\"",
        src = quote(mkv),
        dest = quote(dest),
    );
    let pipeline = gst::parse::launch(&desc)
        .map_err(|e| format!("could not set up the mix: {e}"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| "could not set up the mix")?;
    let mixout = pipeline.by_name("mixout").ok_or("mix output missing")?;
    let mmux = pipeline.by_name("mmux").ok_or("mix muxer missing")?;
    let enc = encode::audio_chain(codec, kbps)?;
    pipeline.add_many(&enc).map_err(|e| e.to_string())?;
    let mut chain = vec![mixout];
    chain.extend(enc);
    chain.push(mmux);
    gst::Element::link_many(&chain).map_err(|_| "could not connect the mix encoder")?;
    run_to_end(&pipeline, "mixing")
}

/// Stage 2: copy video, the mix (if any) and the original audio tracks into `dest`.
fn copy_into(mkv: &Path, mix: Option<&Path>, dest: &Path, mp4: bool) -> Result<(), String> {
    let pipeline = gst::Pipeline::new();
    let mux = if mp4 {
        let m = make("mp4mux")?;
        if m.find_property("faststart").is_some() {
            m.set_property("faststart", true);
        }
        m
    } else {
        let m = make("matroskamux")?;
        if m.find_property("writing-app").is_some() {
            m.set_property("writing-app", "Lucentia");
        }
        m
    };
    let sink = make("filesink")?;
    sink.set_property("location", dest.to_string_lossy().to_string());
    pipeline
        .add_many([&mux, &sink])
        .map_err(|e| e.to_string())?;
    mux.link(&sink)
        .map_err(|_| "could not write the finished file")?;

    // Request the muxer's tracks in the order we want them in the file.
    let video_pad = mux
        .request_pad_simple("video_%u")
        .ok_or("the muxer refused the video track")?;
    let mix_pad = match mix {
        Some(_) => Some(
            mux.request_pad_simple("audio_%u")
                .ok_or("the muxer refused the mix track")?,
        ),
        None => None,
    };

    // The recording: video to its pad, audio tracks each to a new pad, in order.
    let main_demux = add_reader(&pipeline, mkv)?;
    {
        let pipeline_weak = pipeline.downgrade();
        let mux_weak = mux.downgrade();
        let video_pad = Mutex::new(Some(video_pad));
        let audio_n = Mutex::new(0usize);
        main_demux.connect_pad_added(move |_, pad| {
            let (Some(pipeline), Some(mux)) = (pipeline_weak.upgrade(), mux_weak.upgrade()) else {
                return;
            };
            let r = if pad.name().starts_with("video") {
                match video_pad.lock().unwrap().take() {
                    Some(target) => link_copy(&pipeline, pad, &target, None),
                    None => Err("skipping an extra video track".to_string()),
                }
            } else if pad.name().starts_with("audio") {
                let n = {
                    let mut c = audio_n.lock().unwrap();
                    *c += 1;
                    *c - 1
                };
                match mux.request_pad_simple("audio_%u") {
                    Some(target) => link_copy(&pipeline, pad, &target, TRACK_NAMES.get(n).copied()),
                    None => Err("the muxer refused an audio track".to_string()),
                }
            } else {
                Err(format!("skipping track {}", pad.name()))
            };
            if let Err(e) = r {
                log::warn(&format!("finishing: {e}"));
            }
        });
    }

    // The mix file: its one audio track to the mix pad.
    if let (Some(path), Some(target)) = (mix, mix_pad) {
        let mix_demux = add_reader(&pipeline, path)?;
        let pipeline_weak = pipeline.downgrade();
        let target = Mutex::new(Some(target));
        mix_demux.connect_pad_added(move |_, pad| {
            let Some(pipeline) = pipeline_weak.upgrade() else {
                return;
            };
            if let Some(t) = target.lock().unwrap().take() {
                if let Err(e) = link_copy(&pipeline, pad, &t, Some("Game + Mic")) {
                    log::warn(&format!("finishing: {e}"));
                }
            }
        });
    }

    run_to_end(&pipeline, "finishing")
}

/// filesrc -> matroskademux, added to the pipeline. Returns the demuxer.
fn add_reader(pipeline: &gst::Pipeline, path: &Path) -> Result<gst::Element, String> {
    let src = make("filesrc")?;
    src.set_property("location", path.to_string_lossy().to_string());
    let demux = make("matroskademux")?;
    pipeline
        .add_many([&src, &demux])
        .map_err(|e| e.to_string())?;
    src.link(&demux)
        .map_err(|_| format!("could not read {}", path.display()))?;
    Ok(demux)
}

/// demux pad -> queue -> [track name] -> muxer pad.
fn link_copy(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    target: &gst::Pad,
    name: Option<&str>,
) -> Result<(), String> {
    let mut chain = vec![make("queue")?];
    if let Some(n) = name {
        let t = make("taginject")?;
        t.set_property("tags", format!("title=\"{n}\""));
        chain.push(t);
    }
    pipeline.add_many(&chain).map_err(|e| e.to_string())?;
    if chain.len() > 1 {
        gst::Element::link_many(&chain).map_err(|_| "could not link elements".to_string())?;
    }
    for el in &chain {
        el.sync_state_with_parent().map_err(|e| e.to_string())?;
    }
    pad.link(&chain[0].static_pad("sink").unwrap())
        .map_err(|e| format!("could not read a track: {e:?}"))?;
    chain
        .last()
        .unwrap()
        .static_pad("src")
        .unwrap()
        .link(target)
        .map_err(|e| format!("could not copy a track: {e:?}"))?;
    Ok(())
}

fn run_to_end(pipeline: &gst::Pipeline, what: &str) -> Result<(), String> {
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| format!("could not start {what}"))?;
    let bus = pipeline.bus().ok_or("no bus")?;
    // Long recordings take a while, so instead of a fixed time limit, give up only if
    // the position stops moving for 30 seconds.
    let mut last_pos = None;
    let mut stalled_since = std::time::Instant::now();
    let result = loop {
        if let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(1)) {
            match msg.view() {
                gst::MessageView::Eos(_) => break Ok(()),
                gst::MessageView::Error(e) => {
                    log::debug(&format!("{:?}", e.debug()));
                    break Err(format!("{what}: {}", e.error()));
                }
                _ => continue,
            }
        }
        let pos = pipeline.query_position::<gst::ClockTime>();
        if pos != last_pos {
            last_pos = pos;
            stalled_since = std::time::Instant::now();
        } else if stalled_since.elapsed().as_secs() >= 30 {
            break Err(format!("{what} stopped making progress"));
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    result
}
