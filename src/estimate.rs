//! File size estimates and free disk space.

use std::path::Path;

use crate::encode::{AudioCodec, Codec, RateControl};

/// Expected data rate in bytes per second: a single value for fixed bitrates and
/// raw video, a range for VBR and constant quality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rate {
    pub low: f64,
    pub high: f64,
}

impl Rate {
    pub fn exact(&self) -> bool {
        (self.high - self.low).abs() < 1.0
    }
}

pub struct Inputs {
    pub codec: Codec,
    pub rate: RateControl,
    pub width: i32,
    pub height: i32,
    pub fps: f64,
    pub ten_bit: bool,
    pub audio_codec: AudioCodec,
    pub audio_kbps: u32,
    /// Number of audio tracks (game, mic, mix).
    pub audio_tracks: u32,
}

pub fn rate(i: &Inputs) -> Rate {
    let per_audio = match i.audio_codec {
        AudioCodec::Pcm => 48_000.0 * 2.0 * 3.0, // 48 kHz, stereo, 24-bit
        _ => i.audio_kbps as f64 * 1000.0 / 8.0,
    };
    let audio = per_audio * i.audio_tracks as f64;

    if i.codec == Codec::Raw {
        // 4:2:0 = 1.5 samples per pixel; 10-bit P010 stores each sample in 2 bytes.
        let bytes_per_sample = if i.ten_bit { 2.0 } else { 1.0 };
        let v = i.width as f64 * i.height as f64 * 1.5 * bytes_per_sample * i.fps;
        return Rate {
            low: v + audio,
            high: v + audio,
        };
    }

    let kbps_to_bytes = |k: u32| k as f64 * 1000.0 / 8.0;
    let (low, high) = match i.rate {
        RateControl::Cbr { kbps } => (kbps_to_bytes(kbps), kbps_to_bytes(kbps)),
        // A VBR average drifts below target on easy scenes, never above the ceiling.
        RateControl::Vbr { kbps, max_kbps } => (
            kbps_to_bytes(kbps) * 0.7,
            kbps_to_bytes(kbps.max(1).min(max_kbps)),
        ),
        RateControl::Quality { q } => quality_guess(i, q),
    };
    Rate {
        low: low + audio,
        high: high + audio,
    }
}

/// Rough range for constant quality, from bits per pixel typical of game footage.
/// Each step of 6 in q roughly halves the size. Real numbers vary a lot with the game,
/// which is why the live stats show the actual rate once recording starts.
fn quality_guess(i: &Inputs, q: u32) -> (f64, f64) {
    let pixels_per_sec = i.width as f64 * i.height as f64 * i.fps;
    let efficiency = match i.codec {
        Codec::H264 => 1.0,
        Codec::Hevc => 0.7,
        Codec::Av1 => 0.6,
        Codec::Raw => 1.0,
    };
    // At q=23, H.264 game footage lands around 0.05 to 0.2 bits per pixel.
    let scale = 2f64.powf((23.0 - q as f64) / 6.0) * efficiency;
    let to_bytes = |bpp: f64| pixels_per_sec * bpp * scale / 8.0;
    (to_bytes(0.05), to_bytes(0.2))
}

pub fn human_bytes(b: f64) -> String {
    const GB: f64 = 1e9;
    const MB: f64 = 1e6;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.0} MB", b / MB)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

pub fn human_duration(secs: f64) -> String {
    if secs >= 100.0 * 3600.0 {
        return "100h+".to_string();
    }
    let s = secs.max(0.0) as u64;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// "≈ 300 MB per minute, 18.0 GB per hour" or a range for variable rates.
pub fn describe(r: Rate) -> String {
    let show = |per_sec: f64| (human_bytes(per_sec * 60.0), human_bytes(per_sec * 3600.0));
    if r.exact() {
        let (m, h) = show(r.high);
        format!("≈ {m} per minute, {h} per hour")
    } else {
        let (ml, hl) = show(r.low);
        let (mh, hh) = show(r.high);
        format!("≈ {ml} to {mh} per minute, {hl} to {hh} per hour (depends on the game)")
    }
}

/// Free space in bytes on the drive holding `path`.
#[cfg(unix)]
pub fn free_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(Path::new("."))
    };
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c is a valid NUL-terminated path and st is a properly sized buffer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn free_space(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(codec: Codec, rate: RateControl) -> Inputs {
        Inputs {
            codec,
            rate,
            width: 1920,
            height: 1080,
            fps: 60.0,
            ten_bit: false,
            audio_codec: AudioCodec::Aac,
            audio_kbps: 320,
            audio_tracks: 0,
        }
    }

    #[test]
    fn cbr_40_mbps_is_300_mb_per_minute() {
        let r = rate(&inputs(Codec::Hevc, RateControl::Cbr { kbps: 40_000 }));
        assert!(r.exact());
        assert!((r.high * 60.0 - 300e6).abs() < 1e3);
    }

    #[test]
    fn raw_1080p60_is_about_11_gb_per_minute() {
        let r = rate(&inputs(Codec::Raw, RateControl::Cbr { kbps: 0 }));
        let per_min = r.high * 60.0 / 1e9;
        assert!((per_min - 11.2).abs() < 0.1, "{per_min}");
    }

    #[test]
    fn formats_durations_and_sizes() {
        assert_eq!(human_duration(3725.0), "1h 02m");
        assert_eq!(human_duration(65.0), "1m 05s");
        assert_eq!(human_duration(1e12), "100h+");
        assert_eq!(human_bytes(18e9), "18.0 GB");
        assert_eq!(human_bytes(300e6), "300 MB");
    }
}
