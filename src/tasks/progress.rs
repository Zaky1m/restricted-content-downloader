//! The progress message of one transfer: a bar edited at most every twelve seconds.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use super::{TaskCtx, status::Status};

const MIN_GAP: Duration = Duration::from_secs(12);

/// Transfers smaller than this show no progress bar.
pub const THRESHOLD: u64 = 10 * 1024 * 1024;

/// Which word the bar shows, as a locale key.
#[derive(Clone, Copy)]
pub enum Action {
    Downloading,
    Uploading,
}

impl Action {
    fn key(self) -> &'static str {
        match self {
            Action::Downloading => "downloads.progress.downloading",
            Action::Uploading => "downloads.progress.uploading",
        }
    }
}

pub struct Progress {
    tx: mpsc::UnboundedSender<(u64, u64)>,
}

impl Progress {
    /// Reports `(done, total)`; cheap enough to call after every chunk.
    pub fn report(&self, done: u64, total: u64) {
        let _ = self.tx.send((done, total));
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<(u64, u64)> {
        self.tx.clone()
    }
}

pub fn bar(percentage: f64) -> String {
    let filled = ((percentage / 5.0) as usize).min(20);
    format!("{}{}", "█".repeat(filled), "░".repeat(20 - filled))
}

/// Starts the task that turns reports into edits of `status`. It ends when the `Progress` is dropped.
pub fn start(ctx: &Arc<TaskCtx>, status: Arc<Status>, action: Action) -> Progress {
    let (tx, mut rx) = mpsc::unbounded_channel::<(u64, u64)>();
    let ctx = ctx.clone();
    let began = Instant::now();
    tokio::spawn(async move {
        let mut last_edit: Option<Instant> = None;
        while let Some((mut done, mut total)) = rx.recv().await {
            while let Ok(next) = rx.try_recv() {
                (done, total) = next;
            }
            let percentage = if total > 0 {
                (done as f64 * 100.0 / total as f64).clamp(0.0, 100.0)
            } else {
                0.0
            };
            if percentage < 100.0 && last_edit.is_some_and(|at| at.elapsed() < MIN_GAP) {
                continue;
            }
            if ctx.cancel.is_cancelled() {
                continue;
            }
            let elapsed = began.elapsed().as_secs_f64().max(0.001);
            let speed = done as f64 / elapsed;
            let eta = if speed > 0.0 && total > 0 {
                (total.saturating_sub(done) as f64 / speed) as i64
            } else {
                0
            };
            let bot = &ctx.bot;
            let body = bot.t(
                &ctx.lang,
                "downloads.progress.body",
                &[
                    ("action", &bot.t(&ctx.lang, action.key(), &[])),
                    ("progress_bar", &bar(percentage)),
                    ("current_size", &bot.size(&ctx.lang, done)),
                    ("total_size", &bot.size(&ctx.lang, total)),
                    ("percentage", &crate::format::py_float_2(percentage)),
                    ("speed", &bot.size(&ctx.lang, speed as u64)),
                    ("eta", &bot.duration(&ctx.lang, eta)),
                ],
            );
            let text = match status.header() {
                Some(header) => format!("{header}\n\n{body}"),
                None => body,
            };
            if status.edit(bot, &text, None).await {
                last_edit = Some(Instant::now());
            }
        }
    });
    Progress { tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_fills_in_twentieths() {
        assert_eq!(bar(0.0), "░".repeat(20));
        assert_eq!(bar(50.0), format!("{}{}", "█".repeat(10), "░".repeat(10)));
        assert_eq!(bar(100.0), "█".repeat(20));
        assert_eq!(bar(7.0), format!("█{}", "░".repeat(19)));
    }
}
