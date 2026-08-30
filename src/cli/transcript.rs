//! `voxtype transcript` — read back the journal of dictated text.

use clap::Subcommand;

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum TranscriptAction {
    /// Print the most recent transcript
    Last {
        /// Copy it to the clipboard instead of printing it
        #[arg(long)]
        copy: bool,

        /// Include transcripts that were discarded by a cancel
        #[arg(long)]
        include_cancelled: bool,
    },
    /// List recent transcripts, oldest first
    List {
        /// How many to show
        #[arg(short = 'n', long, value_name = "COUNT", default_value_t = 10)]
        count: usize,
    },
    /// Print the path of the journal file
    Path,
}
