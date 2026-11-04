//! Colored terminal output.
//!
//! Red = errors, bold red = crash headers, cyan = stack frames, yellow = warnings and
//! known-expected issues, green = success. Colors only appear on a real terminal;
//! piped or saved output is plain text. Set NO_COLOR=1 or LUCENTIA_NO_COLOR=1 to turn
//! colors off everywhere.

use std::io::IsTerminal;
use std::sync::OnceLock;

static COLOR: OnceLock<bool> = OnceLock::new();
static DEBUG: OnceLock<bool> = OnceLock::new();

fn color_on() -> bool {
    *COLOR.get_or_init(|| {
        let disabled = std::env::var_os("NO_COLOR").is_some()
            || std::env::var_os("LUCENTIA_NO_COLOR").is_some();
        !disabled && std::io::stdout().is_terminal()
    })
}

/// Turn debug mode on (from --debug or LUCENTIA_DEBUG=1). Call once at startup.
pub fn init_debug(enabled: bool) {
    let _ = DEBUG.set(enabled);
}

pub fn debug_on() -> bool {
    *DEBUG.get().unwrap_or(&false)
}

fn paint(code: &str, text: &str) -> String {
    if color_on() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn red(t: &str) -> String {
    paint("31", t)
}
#[allow(dead_code)] // for crash report headers, used once recording lands
pub fn bold_red(t: &str) -> String {
    paint("1;31", t)
}
pub fn cyan(t: &str) -> String {
    paint("36", t)
}
pub fn yellow(t: &str) -> String {
    paint("33", t)
}
pub fn green(t: &str) -> String {
    paint("32", t)
}
pub fn bold(t: &str) -> String {
    paint("1", t)
}
pub fn dim(t: &str) -> String {
    paint("2", t)
}

pub fn error(msg: &str) {
    eprintln!("{} {msg}", red("error:"));
}
pub fn warn(msg: &str) {
    println!("{} {msg}", yellow("warning:"));
}
pub fn ok(msg: &str) {
    println!("{} {msg}", green("ok:"));
}

/// Printed only in debug mode.
pub fn debug(msg: &str) {
    if debug_on() {
        println!("{} {msg}", cyan("debug:"));
    }
}
