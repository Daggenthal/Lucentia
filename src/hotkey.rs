//! Start/stop recording from anywhere, even while a game has focus.
//!
//! Two ways in:
//! 1. The desktop's global shortcuts portal (KDE, GNOME 48+, Hyprland and others on
//!    Wayland or X11). Lucentia asks for Alt+F9; the desktop shows its own dialog
//!    to confirm or change it, and remembers the choice.
//! 2. A command: `lucentia toggle-record` tells the running app to start or stop.
//!    Bind it to any key in your desktop's keyboard settings, a Steam Input button,
//!    or a script. This works everywhere, including where there's no portal.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use crate::log;

const SHORTCUT_ID: &str = "toggle-record";
/// The trigger Lucentia suggests (ShadowPlay's record key), in the XDG shortcuts
/// format. The desktop can change it.
const PREFERRED: &str = "ALT+F9";

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("lucentia.sock")
}

/// Listen for `lucentia toggle-record`. `toggle` runs on a background thread.
pub fn listen(toggle: impl Fn() + Send + 'static) {
    let path = socket_path();
    // A socket left from a crash: remove it, unless another Lucentia is answering.
    if UnixStream::connect(&path).is_err() {
        let _ = std::fs::remove_file(&path);
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            log::warn(&format!(
                "record command unavailable ({}: {e}); is another Lucentia running?",
                path.display()
            ));
            return;
        }
    };
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            if BufReader::new(&conn).read_line(&mut line).is_ok() && line.trim() == "toggle" {
                log::debug("record toggled by command");
                toggle();
                let _ = (&conn).write_all(b"ok\n");
            }
        }
    });
}

/// `lucentia toggle-record`: ask the running app to start or stop recording.
pub fn send_toggle() -> Result<(), String> {
    let mut s = UnixStream::connect(socket_path())
        .map_err(|_| "Lucentia isn't running (open the app first)".to_string())?;
    s.write_all(b"toggle\n").map_err(|e| e.to_string())?;
    Ok(())
}

pub fn remove_socket() {
    let _ = std::fs::remove_file(socket_path());
}

/// Register the global shortcut with the desktop portal. Reports the bound key
/// (e.g. "Alt+F9") through `bound`, or None if the desktop has no portal for it, and
/// calls `toggle` each time it's pressed. Runs on its own thread.
pub fn register(
    bound: impl Fn(Option<String>) + Send + 'static,
    toggle: impl Fn() + Send + 'static,
) {
    std::thread::spawn(move || {
        let result = pollster::block_on(async {
            use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};
            use futures_util::StreamExt;

            let portal = GlobalShortcuts::new().await?;
            let session = portal.create_session(Default::default()).await?;
            let shortcut = NewShortcut::new(SHORTCUT_ID, "Start or stop recording")
                .preferred_trigger(PREFERRED);
            let request = portal
                .bind_shortcuts(&session, &[shortcut], None, Default::default())
                .await?;
            let response = request.response()?;
            let key = response
                .shortcuts()
                .iter()
                .find(|s| s.id() == SHORTCUT_ID)
                .map(|s| s.trigger_description().to_string())
                .filter(|t| !t.is_empty());
            log::debug(&format!("global shortcut: {key:?}"));
            bound(Some(key.unwrap_or_else(|| "set in your desktop's settings".into())));

            let mut presses = portal.receive_activated().await?;
            while let Some(a) = presses.next().await {
                if a.shortcut_id() == SHORTCUT_ID {
                    log::debug("record toggled by shortcut");
                    toggle();
                }
            }
            Ok::<(), ashpd::Error>(())
        });
        if let Err(e) = result {
            log::debug(&format!("no global shortcuts portal: {e}"));
            bound(None);
        }
    });
}
