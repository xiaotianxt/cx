use std::io::IsTerminal;
use std::time::Duration;

use indicatif::ProgressBar;
use indicatif::ProgressDrawTarget;
use indicatif::ProgressStyle;

use crate::selector::SlotQueryProgress as SlotQueryProgressSink;
use crate::usage::SlotResult;

const PROGRESS_REFRESH_HZ: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressOptions {
    enabled: bool,
}

impl ProgressOptions {
    pub fn for_human_output(disabled: bool) -> Self {
        Self {
            enabled: !disabled && terminal_progress_allowed(),
        }
    }
}

#[derive(Debug)]
pub struct ProgressHandle {
    bar: Option<ProgressBar>,
}

impl ProgressHandle {
    pub fn count(options: ProgressOptions, len: usize, message: impl Into<String>) -> Self {
        if !options.enabled || len == 0 {
            return Self { bar: None };
        }

        let bar = ProgressBar::with_draw_target(
            Some(len as u64),
            ProgressDrawTarget::stderr_with_hz(PROGRESS_REFRESH_HZ),
        );
        if let Ok(style) = ProgressStyle::with_template("{spinner} {msg} [{bar:20}] {pos}/{len}") {
            bar.set_style(style.progress_chars("=>-").tick_chars("-\\|/"));
        }
        bar.set_message(message.into());
        bar.enable_steady_tick(Duration::from_millis(100));
        bar.force_draw();
        Self { bar: Some(bar) }
    }

    pub fn reset_count(&mut self, len: usize, message: impl Into<String>) {
        if let Some(bar) = &self.bar {
            bar.set_length(len as u64);
            bar.set_position(0);
            bar.set_message(message.into());
            bar.force_draw();
        }
    }

    pub fn inc(&mut self, delta: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(delta);
        }
    }

    pub fn finish_and_clear(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}

impl Drop for ProgressHandle {
    fn drop(&mut self) {
        self.finish_and_clear();
    }
}

#[derive(Debug)]
pub struct SlotStatusProgress {
    options: ProgressOptions,
    handle: ProgressHandle,
}

impl SlotStatusProgress {
    pub fn for_status_command(json: bool, disabled: bool) -> Self {
        Self {
            options: ProgressOptions::for_human_output(json || disabled),
            handle: ProgressHandle { bar: None },
        }
    }

    pub fn finish_and_clear(&mut self) {
        self.handle.finish_and_clear();
    }
}

impl SlotQueryProgressSink for SlotStatusProgress {
    fn started(&mut self, total: usize) {
        self.handle = ProgressHandle::count(self.options, total, "checking slots");
    }

    fn slot_checked(&mut self, _result: &SlotResult) {
        self.handle.inc(1);
    }

    fn retry_started(&mut self, attempt: usize, total_attempts: usize, total: usize) {
        self.handle.reset_count(
            total,
            format!("retrying transient failures {attempt}/{total_attempts}"),
        );
    }
}

fn terminal_progress_allowed() -> bool {
    std::env::var_os("CX_NO_PROGRESS").is_none()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
}
