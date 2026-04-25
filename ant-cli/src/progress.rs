//! Process-wide progress UI plumbing.
//!
//! The CLI uses `indicatif` spinners and progress bars for long-running operations
//! (network connection, file uploads/downloads). Tracing logs and progress bars
//! both write to stderr, so without coordination they garble each other when both
//! are active (e.g. `ant -v file upload ...`).
//!
//! This module exposes a single `MultiProgress` that all bars/spinners attach to,
//! and a tracing writer that wraps each write in `MultiProgress::suspend()` so log
//! lines render cleanly above any active bars.

use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing_subscriber::fmt::MakeWriter;

static MULTI: OnceLock<MultiProgress> = OnceLock::new();

/// The shared `MultiProgress` instance. Created on first access.
pub fn multi() -> &'static MultiProgress {
    MULTI.get_or_init(MultiProgress::new)
}

/// Create a styled spinner attached to the shared `MultiProgress`.
/// Hidden when stderr is not a terminal (piped output).
pub fn new_spinner(msg: &str) -> ProgressBar {
    let pb = if io::stderr().is_terminal() {
        multi().add(ProgressBar::new_spinner())
    } else {
        ProgressBar::hidden()
    };
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("valid template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Attach a pre-built `ProgressBar` to the shared `MultiProgress`.
/// When stderr is not a terminal, returns a hidden bar instead.
pub fn attach(pb: ProgressBar) -> ProgressBar {
    if io::stderr().is_terminal() {
        multi().add(pb)
    } else {
        ProgressBar::hidden()
    }
}

/// Tracing writer that suspends active progress bars while writing log lines.
#[derive(Clone)]
pub struct ProgressAwareWriter;

impl Write for ProgressAwareWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = buf.len();
        multi().suspend(|| io::stderr().write_all(buf))?;
        Ok(len)
    }

    fn flush(&mut self) -> io::Result<()> {
        multi().suspend(|| io::stderr().flush())
    }
}

impl<'a> MakeWriter<'a> for ProgressAwareWriter {
    type Writer = ProgressAwareWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ProgressAwareWriter
    }
}
