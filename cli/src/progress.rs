//! Terminal progress bars for `nuxd get`/`resume` — one per file, drawn
//! from periodic `Engine::snapshot()` polls (§19: 4 Hz) rather than a
//! live byte-level event stream, which the engine doesn't emit yet.

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use nuxd_core::model::{FileId, FileStatus, Package};
use std::collections::HashMap;

pub struct Bars {
    multi: MultiProgress,
    by_file: HashMap<FileId, ProgressBar>,
}

fn style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:<24} [{bar:32}] {bytes:>10}/{total_bytes:<10} {msg}")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=> ")
}

impl Bars {
    #[must_use]
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
            by_file: HashMap::new(),
        }
    }

    /// Reflects one package's current file progress in its bars, creating
    /// one per file the first time it's seen.
    pub fn update(&mut self, package: &Package) {
        for file in &package.files {
            let durable: u64 = file.parts.iter().map(|part| part.durable).sum();
            let total = file.expected_size.unwrap_or(durable.max(1));

            let bar = self.by_file.entry(file.id).or_insert_with(|| {
                let bar = self.multi.add(ProgressBar::new(total));
                bar.set_style(style());
                bar.set_prefix(truncate(&file.name, 22));
                bar
            });
            bar.set_length(total);
            bar.set_position(durable);
            bar.set_message(status_word(&file.status));
            if matches!(file.status, FileStatus::Verified | FileStatus::Failed(_)) {
                bar.finish();
            }
        }
    }

    pub fn finish(&self) {
        for bar in self.by_file.values() {
            if !bar.is_finished() {
                bar.finish();
            }
        }
    }

    pub fn finish_interrupted(&self) {
        for bar in self.by_file.values() {
            bar.abandon_with_message("interrupted");
        }
    }
}

impl Default for Bars {
    fn default() -> Self {
        Self::new()
    }
}

fn status_word(status: &FileStatus) -> &'static str {
    match status {
        FileStatus::Unprobed | FileStatus::Queued => "queued",
        FileStatus::Preparing => "preparing",
        FileStatus::Active => "downloading",
        FileStatus::Complete => "complete",
        FileStatus::Verified => "done",
        FileStatus::Failed(_) => "failed",
    }
}

fn truncate(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        name.to_string()
    } else {
        let mut short: String = name.chars().take(max.saturating_sub(1)).collect();
        short.push('…');
        short
    }
}
