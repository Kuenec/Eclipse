use std::sync::mpsc::Sender;

use tracing::Level;

use crate::diagnostics::record_status;

const MEBIBYTE: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusUpdate {
    Step(String),

    Transfer { done: u64, total: Option<u64> },

    Warning(String),
}

pub struct StatusSink {
    window: Option<Sender<StatusUpdate>>,
}

impl StatusSink {
    pub fn terminal() -> Self {
        Self { window: None }
    }

    pub fn with_window(updates: Sender<StatusUpdate>) -> Self {
        Self {
            window: Some(updates),
        }
    }

    pub fn step(&self, text: impl Into<String>) {
        let text = text.into();
        println!("# {text}");
        record_status(Level::INFO, &text);
        self.show(StatusUpdate::Step(text));
    }

    pub fn outcome(&self, text: impl Into<String>) {
        let text = text.into();
        println!("{text}");
        record_status(Level::INFO, &text);
        self.show(StatusUpdate::Step(text));
    }

    pub fn warning(&self, text: impl Into<String>) {
        let text = text.into();
        eprintln!("# WARNING: {text}");
        record_status(Level::WARN, &text);
        self.show(StatusUpdate::Warning(text));
    }

    pub fn transfer(&self, done: u64, total: Option<u64>) {
        self.show(StatusUpdate::Transfer { done, total });
    }

    fn show(&self, update: StatusUpdate) {
        let Some(window) = &self.window else {
            return;
        };
        if window.send(update).is_err() {
            tracing::debug!(
                "the launch window has closed; status is reported on the terminal only"
            );
        }
    }
}

fn mebibytes(bytes: u64) -> f64 {
    bytes as f64 / MEBIBYTE
}

pub fn transfer_text(done: u64, total: Option<u64>) -> String {
    match total {
        Some(total) if total > 0 => format!(
            "Downloaded {:.1} of {:.1} MiB ({}%)",
            mebibytes(done),
            mebibytes(total),
            done.saturating_mul(100) / total
        ),
        _ => format!("Downloaded {:.1} MiB", mebibytes(done)),
    }
}

pub fn continuation_text(done: u64) -> String {
    format!("Continuing the download from {:.1} MiB…", mebibytes(done))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_are_reported_in_mebibytes_and_percent() {
        assert_eq!(
            transfer_text(120 * 1024 * 1024, Some(240 * 1024 * 1024)),
            "Downloaded 120.0 of 240.0 MiB (50%)"
        );
        assert_eq!(
            transfer_text(1024 * 1024 + 512 * 1024, None),
            "Downloaded 1.5 MiB"
        );
        assert_eq!(transfer_text(0, Some(0)), "Downloaded 0.0 MiB");
        assert_eq!(
            continuation_text(120 * 1024 * 1024),
            "Continuing the download from 120.0 MiB…"
        );
    }

    #[test]
    fn window_updates_follow_the_order_they_were_reported_in() {
        let (updates, received) = std::sync::mpsc::channel();
        let status = StatusSink::with_window(updates);
        status.step("Checking APKCombo");
        status.transfer(5, Some(10));
        status.warning("could not update Roblox");
        status.outcome("Roblox 2.740.931 is up to date");
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [
                StatusUpdate::Step("Checking APKCombo".to_owned()),
                StatusUpdate::Transfer {
                    done: 5,
                    total: Some(10)
                },
                StatusUpdate::Warning("could not update Roblox".to_owned()),
                StatusUpdate::Step("Roblox 2.740.931 is up to date".to_owned()),
            ]
        );
    }
}
