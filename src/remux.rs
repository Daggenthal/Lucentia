//! Convert a finished MKV recording to MP4 without re-encoding.
//!
//! Every track is copied as-is (video, game, mic, mix), so this takes seconds, not
//! minutes, and loses no quality. The MP4 is written "fast start" (index at the front),
//! so it starts playing immediately when uploaded or streamed.

use gst::prelude::*;
use gstreamer as gst;
use std::path::Path;

use crate::log;
use crate::preview::make;

pub fn mkv_to_mp4(mkv: &Path, mp4: &Path) -> Result<(), String> {
    let pipeline = gst::Pipeline::new();
    let src = make("filesrc")?;
    src.set_property("location", mkv.to_string_lossy().to_string());
    let demux = make("matroskademux")?;
    let mux = make("mp4mux")?;
    if mux.find_property("faststart").is_some() {
        mux.set_property("faststart", true);
    }
    let sink = make("filesink")?;
    sink.set_property("location", mp4.to_string_lossy().to_string());
    pipeline
        .add_many([&src, &demux, &mux, &sink])
        .map_err(|e| e.to_string())?;
    src.link(&demux).map_err(|_| "could not read the MKV")?;
    mux.link(&sink).map_err(|_| "could not write the MP4")?;

    // The demuxer announces each track as it finds it; give each one a queue and a
    // pad on the MP4 muxer.
    let pipeline_weak = pipeline.downgrade();
    let mux_weak = mux.downgrade();
    demux.connect_pad_added(move |_, pad| {
        let (Some(pipeline), Some(mux)) = (pipeline_weak.upgrade(), mux_weak.upgrade()) else {
            return;
        };
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let name = caps
            .structure(0)
            .map(|s| s.name().to_string())
            .unwrap_or_default();
        let template = if name.starts_with("audio/") {
            "audio_%u"
        } else {
            "video_%u"
        };
        let Ok(queue) = make("queue") else { return };
        if pipeline.add(&queue).is_err() {
            return;
        }
        let _ = queue.sync_state_with_parent();
        let Some(mux_pad) = mux.request_pad_simple(template) else {
            log::warn(&format!("MP4 can't hold a {name} track, leaving it out"));
            return;
        };
        if pad.link(&queue.static_pad("sink").unwrap()).is_err()
            || queue.static_pad("src").unwrap().link(&mux_pad).is_err()
        {
            log::warn(&format!("could not copy the {name} track into the MP4"));
        } else {
            log::debug(&format!("MP4: copying {name} track"));
        }
    });

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|_| "could not start the conversion".to_string())?;
    let bus = pipeline.bus().ok_or("no bus")?;
    let result = loop {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(600)) else {
            break Err("conversion timed out".to_string());
        };
        match msg.view() {
            gst::MessageView::Eos(_) => break Ok(()),
            gst::MessageView::Error(e) => {
                log::debug(&format!("{:?}", e.debug()));
                break Err(e.error().to_string());
            }
            _ => {}
        }
    };
    let _ = pipeline.set_state(gst::State::Null);
    if result.is_err() {
        let _ = std::fs::remove_file(mp4);
    }
    result
}
