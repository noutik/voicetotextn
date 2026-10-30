//! Tiny append-only log in $XDG_STATE_HOME/voicetotextn.log (niri drops stderr of spawned programs).

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

macro_rules! log {
    ($($arg:tt)*) => { $crate::log::write(format_args!($($arg)*)) };
}

pub fn write(msg: std::fmt::Arguments) {
    let ms = START.get_or_init(Instant::now).elapsed().as_millis();
    let line = format!("[{} +{ms}ms] {msg}\n", std::process::id());
    eprint!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Marks the session start so log lines show milliseconds since the key press.
pub fn start() {
    START.get_or_init(Instant::now);
}

fn path() -> PathBuf {
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state"));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("voicetotextn.log")
}
