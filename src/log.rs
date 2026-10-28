use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const MAX_BYTES: u64 = 1024 * 1024;

fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/godview"))
        })?;
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join("godview.log"))
}

/// The screen belongs to the TUI, so warnings go to a log file, dropped once it passes 1 MiB.
pub fn warn(message: &str) {
    let Some(path) = path() else { return };
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        return;
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{message}");
    }
}
