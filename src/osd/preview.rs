//! Live transcript preview channel (daemon → OSD).
//!
//! While a streaming transcription session is active the daemon publishes
//! the decoded-so-far text here and the OSD renders it. The transport is a
//! single small file under the runtime directory, written atomically via a
//! sibling temp file + rename, exactly like the transcript sidecar in
//! `daemon.rs`. Readers therefore never observe a half-written preview.
//!
//! # Why a file and not the audio socket
//!
//! `audio.sock` carries a fixed 16-byte [`AudioFrame`](crate::audio::levels::AudioFrame)
//! stream that `voxtype-audio-bridge` and the Quickshell QML modules parse
//! by offset. Introducing a tagged envelope there would break both for a
//! payload that arrives at a handful of hertz. A file mirrors the existing
//! `state_file` pattern the daemon already uses for Waybar, costs no
//! protocol version negotiation, and any frontend can adopt it with three
//! lines of code.
//!
//! The OSD polls this only while frames are arriving (i.e. while recording),
//! and at a fraction of its render rate — see `PREVIEW_POLL_MS`.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// How often the OSD re-reads the preview file while a session is active,
/// in milliseconds. Partials arrive at roughly 5–15 Hz depending on chunk
/// size, so 50 ms (20 Hz) samples faster than the producer without burning
/// the render loop on redundant reads.
pub const PREVIEW_POLL_MS: u64 = 50;

/// Default path for the transcript preview file.
pub fn default_preview_path() -> PathBuf {
    Config::runtime_dir().join("preview.txt")
}

/// Sibling temp path used to stage an atomic write. Kept in the same
/// directory as the target so the rename never crosses a filesystem.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Publish `text` as the current preview.
///
/// Best-effort: failures are logged by the caller's discretion and never
/// propagate into the transcription path. An empty `text` still writes an
/// empty file rather than removing it, so the OSD can distinguish
/// "streaming, nothing decoded yet" from "no session" (file absent).
pub fn write_preview(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let staged = temp_sibling(path);
    std::fs::write(&staged, text.as_bytes())?;
    if let Err(e) = std::fs::rename(&staged, path) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

/// Remove the preview file, signalling "no active session" to the OSD.
///
/// Also drops any armed hint: hints are scoped to a session, and one left
/// behind would reappear over the next recording's first frames.
pub fn clear_preview(path: &Path) {
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::debug!("Failed to clear preview file: {}", e);
        }
    }
    let staged = temp_sibling(path);
    if staged.exists() {
        let _ = std::fs::remove_file(&staged);
    }
    clear_hint(&default_hint_path());
}

/// Default path for the transient hint file.
///
/// The hint is a short line the OSD overlays on the transcript panel —
/// currently only "press Esc again to discard". It is a second file rather
/// than a field in the preview because the two have different writers: the
/// daemon owns the preview, while `voxtype record cancel --double-tap`
/// (running as a keybind, with no daemon round-trip) owns the hint.
pub fn default_hint_path() -> PathBuf {
    Config::runtime_dir().join("hint.txt")
}

/// Arm a hint that expires on its own `ttl_ms` from now.
///
/// The deadline is written into the file rather than tracked by a timer,
/// so the writer can be a one-shot process that exits immediately and no
/// one has to come back to clean up.
pub fn write_hint(path: &Path, text: &str, ttl_ms: u64) -> io::Result<()> {
    let deadline = now_ms().saturating_add(ttl_ms);
    let body = format!("{}\n{}", deadline, text);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let staged = temp_sibling(path);
    std::fs::write(&staged, body.as_bytes())?;
    if let Err(e) = std::fs::rename(&staged, path) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    Ok(())
}

/// Read the hint, if one is armed and still inside its deadline.
///
/// An expired hint reads as `None` and is left on disk; the next
/// `clear_preview` sweeps it. Deleting on read would make this racy
/// against a writer arming a fresh hint in the same instant.
pub fn read_hint(path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    let (deadline, text) = body.split_once('\n')?;
    let deadline: u64 = deadline.trim().parse().ok()?;
    if now_ms() >= deadline {
        return None;
    }
    Some(text.to_string())
}

/// Remove the hint file and any staged temp beside it.
pub fn clear_hint(path: &Path) {
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    let staged = temp_sibling(path);
    if staged.exists() {
        let _ = std::fs::remove_file(&staged);
    }
}

/// Milliseconds since the Unix epoch, saturating at 0 if the clock is
/// somehow before it.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Read the current preview.
///
/// Returns `None` when there is no active streaming session (file absent
/// or unreadable) and `Some("")` when a session is live but nothing has
/// been decoded yet.
pub fn read_preview(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "voxtype-preview-test-{}-{}",
            std::process::id(),
            name
        ));
        p
    }

    #[test]
    fn write_then_read_roundtrips() {
        let p = tmp_path("roundtrip");
        write_preview(&p, "hello world").unwrap();
        assert_eq!(read_preview(&p).as_deref(), Some("hello world"));
        clear_preview(&p);
    }

    #[test]
    fn absent_file_reads_none() {
        let p = tmp_path("absent");
        clear_preview(&p);
        assert_eq!(read_preview(&p), None);
    }

    #[test]
    fn empty_preview_is_distinct_from_absent() {
        let p = tmp_path("empty");
        write_preview(&p, "").unwrap();
        assert_eq!(read_preview(&p).as_deref(), Some(""));
        clear_preview(&p);
        assert_eq!(read_preview(&p), None);
    }

    #[test]
    fn clear_removes_staged_temp_too() {
        let p = tmp_path("staged");
        let staged = temp_sibling(&p);
        std::fs::write(&staged, "junk").unwrap();
        clear_preview(&p);
        assert!(!staged.exists());
    }

    #[test]
    fn live_hint_reads_back_and_expired_hint_does_not() {
        let p = tmp_path("hint-live");
        write_hint(&p, "press Esc again to discard", 10_000).unwrap();
        assert_eq!(read_hint(&p).as_deref(), Some("press Esc again to discard"));

        write_hint(&p, "already gone", 0).unwrap();
        assert_eq!(read_hint(&p), None);

        clear_hint(&p);
        assert_eq!(read_hint(&p), None);
    }

    #[test]
    fn hint_text_may_contain_newlines() {
        let p = tmp_path("hint-multiline");
        write_hint(&p, "first\nsecond", 10_000).unwrap();
        assert_eq!(read_hint(&p).as_deref(), Some("first\nsecond"));
        clear_hint(&p);
    }

    #[test]
    fn overwrite_replaces_previous_text() {
        let p = tmp_path("overwrite");
        write_preview(&p, "first").unwrap();
        write_preview(&p, "second").unwrap();
        assert_eq!(read_preview(&p).as_deref(), Some("second"));
        clear_preview(&p);
    }
}
