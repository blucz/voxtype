//! `voxtype transcript last|list|path` — read back the transcript journal.

use voxtype::journal::{self, Entry, Outcome};
use voxtype::TranscriptAction;

/// How many entries to scan when looking for the most recent transcript.
/// `--last` skips cancelled ones by default, so it may have to look past a
/// few; beyond this many the answer is "there isn't one".
const LAST_SCAN_DEPTH: usize = 200;

pub(crate) fn run_transcript_command(action: TranscriptAction) -> anyhow::Result<()> {
    let path = journal::default_path();

    match action {
        TranscriptAction::Path => {
            println!("{}", path.display());
        }

        TranscriptAction::Last {
            copy,
            include_cancelled,
        } => {
            let entries = journal::read_last(&path, LAST_SCAN_DEPTH);
            let found = entries
                .iter()
                .rev()
                .find(|e| include_cancelled || e.outcome == Outcome::Transcribed);

            let Some(entry) = found else {
                anyhow::bail!(
                    "No transcript in the journal ({}). Is output.transcript_journal enabled?",
                    path.display()
                );
            };

            if copy {
                copy_to_clipboard(&entry.text)?;
                eprintln!("Copied {} characters to the clipboard.", entry.text.len());
            } else {
                println!("{}", entry.text);
            }
        }

        TranscriptAction::List { count } => {
            let entries = journal::read_last(&path, count);
            if entries.is_empty() {
                eprintln!("No transcripts in the journal ({}).", path.display());
                return Ok(());
            }
            for entry in &entries {
                println!("{}", summarize(entry));
            }
        }
    }

    Ok(())
}

/// One line per entry: timestamp, outcome marker, and the text with
/// newlines flattened so a multi-line dictation stays on one row.
fn summarize(entry: &Entry) -> String {
    let marker = match entry.outcome {
        Outcome::Transcribed => " ",
        Outcome::Cancelled => "✗",
    };
    let flat = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{} {} {}", entry.at, marker, flat)
}

/// Put `text` on the clipboard via whichever helper this session has.
fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];

    for (bin, args) in candidates {
        let Ok(mut child) = Command::new(bin).args(*args).stdin(Stdio::piped()).spawn() else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
    }

    anyhow::bail!("No clipboard helper found (tried wl-copy, xclip, xsel, pbcopy)")
}
