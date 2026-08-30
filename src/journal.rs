//! Durable journal of everything that was transcribed.
//!
//! Dictation is lossy in a way typing is not: the text exists only in the
//! moment it is delivered, and every way that delivery can go wrong — the
//! wrong window has focus, the paste lands in a field that swallows it, a
//! mis-aimed cancel — destroys it with no undo. Long dictations make that
//! worse, because there is more to lose and more time in which to lose it.
//!
//! So every finished utterance is appended here before it is delivered,
//! and a cancelled one is appended too. `voxtype transcript last` reads it
//! back. The journal is not a feature of the output path; it is the thing
//! that makes the output path survivable.
//!
//! One JSON object per line, newest last. Line-oriented so an append is a
//! single `write` on an `O_APPEND` handle and a crash mid-write can cost
//! at most the entry being written.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Trim the journal once it exceeds this many bytes, keeping
/// [`KEEP_ENTRIES`] entries. Roughly a few thousand dictations.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// How many entries survive a trim.
const KEEP_ENTRIES: usize = 500;

/// How an utterance ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// Delivered to the cursor, clipboard, or a file.
    Transcribed,
    /// Discarded before delivery, by a cancel.
    Cancelled,
}

/// One journal line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// RFC 3339 timestamp, UTC.
    pub at: String,
    pub outcome: Outcome,
    pub text: String,
}

/// Default journal path.
pub fn default_path() -> PathBuf {
    Config::state_dir().join("transcripts.jsonl")
}

/// Append one entry. Blank text is not journalled — an empty utterance is
/// nothing to recover.
///
/// Errors are returned rather than logged so the caller decides; in the
/// daemon that decision is always "warn and carry on", because failing to
/// journal must never cost the user the delivery itself.
pub fn append(path: &Path, outcome: Outcome, text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let entry = Entry {
        at: chrono::Utc::now().to_rfc3339(),
        outcome,
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&entry).map_err(io::Error::other)?;
    line.push('\n');

    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(line.as_bytes())?;

    if f.metadata().map(|m| m.len()).unwrap_or(0) > MAX_BYTES {
        trim(path)?;
    }
    Ok(())
}

/// Rewrite the journal with only its last [`KEEP_ENTRIES`] lines.
///
/// Staged through a sibling temp file and renamed, so a reader either sees
/// the old journal or the new one and never a half-written file.
fn trim(path: &Path) -> io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= KEEP_ENTRIES {
        return Ok(());
    }
    let kept = lines[lines.len() - KEEP_ENTRIES..].join("\n");
    let staged = path.with_extension("jsonl.tmp");
    std::fs::write(&staged, format!("{kept}\n"))?;
    std::fs::rename(&staged, path)
}

/// Read the last `n` entries, oldest first. A missing journal reads as
/// empty; an unparseable line is skipped rather than failing the read, so
/// one bad line cannot hide every good one.
pub fn read_last(path: &Path, n: usize) -> Vec<Entry> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    if entries.len() > n {
        entries.drain(..entries.len() - n);
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("voxtype-journal-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn appends_and_reads_back_in_order() {
        let p = tmp_path("order");
        append(&p, Outcome::Transcribed, "first").unwrap();
        append(&p, Outcome::Cancelled, "second").unwrap();

        let entries = read_last(&p, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "first");
        assert_eq!(entries[0].outcome, Outcome::Transcribed);
        assert_eq!(entries[1].text, "second");
        assert_eq!(entries[1].outcome, Outcome::Cancelled);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_last_returns_the_tail() {
        let p = tmp_path("tail");
        for i in 0..5 {
            append(&p, Outcome::Transcribed, &format!("entry {i}")).unwrap();
        }
        let entries = read_last(&p, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "entry 3");
        assert_eq!(entries[1].text, "entry 4");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn blank_text_is_not_journalled() {
        let p = tmp_path("blank");
        append(&p, Outcome::Transcribed, "   \n ").unwrap();
        assert!(read_last(&p, 10).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn multiline_text_survives_the_round_trip() {
        let p = tmp_path("multiline");
        append(&p, Outcome::Transcribed, "line one\nline two").unwrap();
        assert_eq!(read_last(&p, 10)[0].text, "line one\nline two");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_corrupt_line_does_not_hide_the_rest() {
        let p = tmp_path("corrupt");
        append(&p, Outcome::Transcribed, "good one").unwrap();
        let mut f = OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"{ not json\n").unwrap();
        append(&p, Outcome::Transcribed, "good two").unwrap();

        let entries = read_last(&p, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].text, "good two");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_journal_reads_empty() {
        assert!(read_last(&tmp_path("absent"), 10).is_empty());
    }

    #[test]
    fn trim_keeps_the_newest_entries() {
        let p = tmp_path("trim");
        for i in 0..(KEEP_ENTRIES + 20) {
            append(&p, Outcome::Transcribed, &format!("entry {i}")).unwrap();
        }
        trim(&p).unwrap();
        let entries = read_last(&p, usize::MAX);
        assert_eq!(entries.len(), KEEP_ENTRIES);
        assert_eq!(
            entries.last().unwrap().text,
            format!("entry {}", KEEP_ENTRIES + 19)
        );
        let _ = std::fs::remove_file(&p);
    }
}
