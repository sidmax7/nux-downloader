//! Terminal progress bars for `nuxd get`/`resume` — one per file, drawn
//! from periodic `Engine::snapshot()` polls (§19: 4 Hz) rather than a
//! live byte-level event stream, which the engine doesn't emit yet.

use indicatif::{HumanBytes, MultiProgress, ProgressBar, ProgressStyle};
use nuxd_core::engine::LinkStats;
use nuxd_core::model::{FileId, FileStatus, LinkId, Package};
use nuxd_core::net::health::LinkState;
use std::collections::HashMap;

pub struct Bars {
    multi: MultiProgress,
    by_file: HashMap<FileId, ProgressBar>,
    by_link: HashMap<LinkId, ProgressBar>,
    warned_no_links: bool,
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
            by_link: HashMap::new(),
            warned_no_links: false,
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
            match file.status {
                FileStatus::Verified => bar.finish(),
                // `finish` would draw a failed file as full.
                FileStatus::Failed(_) => bar.abandon(),
                _ => {}
            }
        }
    }

    /// One status line per link: name, speed, bytes, health.
    pub fn update_links(&mut self, links: &[LinkStats]) {
        if links.is_empty() && !self.warned_no_links {
            self.warned_no_links = true;
            let _ = self
                .multi
                .println("nuxd: no usable network links yet — waiting for one (see `nuxd links`)");
        }
        for stats in links {
            let bar = self.by_link.entry(stats.link.id).or_insert_with(|| {
                let bar = self.multi.add(ProgressBar::new_spinner());
                bar.set_style(
                    ProgressStyle::with_template("{msg}")
                        .unwrap_or_else(|_| ProgressStyle::default_spinner()),
                );
                bar
            });
            let health = match stats.state {
                LinkState::Up => "up".to_string(),
                LinkState::Degraded { .. } => "degraded".to_string(),
            };
            bar.set_message(format!(
                "  {:<12} {:>12}/s {:>10}  {} conn  {health}",
                truncate(&stats.link.name, 12),
                HumanBytes(stats.speed as u64).to_string(),
                HumanBytes(stats.bytes).to_string(),
                stats.active,
            ));
        }
        // A link that went away: drop its line.
        self.by_link.retain(|id, bar| {
            let present = links.iter().any(|stats| stats.link.id == *id);
            if !present {
                bar.finish_and_clear();
            }
            present
        });
    }

    pub fn finish(&self) {
        for bar in self.by_link.values() {
            bar.finish();
        }
        for bar in self.by_file.values() {
            if !bar.is_finished() {
                bar.abandon();
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
