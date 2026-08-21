//! Terminal progress rendering shared by every command.
//!
//! One process-wide [`MultiProgress`] owns the stderr draw target so diagnostics written
//! through [`crate::report::logging`] can suspend it for the duration of a line. Every
//! handle returned here degrades to a no-op when rendering is disabled, which keeps call
//! sites free of `if progress_enabled` branches.

use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

const TICK: Duration = Duration::from_millis(120);

const PHASE_TEMPLATE: &str = "{prefix:>12.cyan.bold} [{pos:>2}/{len:<2}] {bar:24.cyan/blue} {msg}";
const COUNT_TEMPLATE: &str =
    "{prefix:>12.green.bold} {bar:24.green/blue} {percent:>3}% {pos}/{len} {msg}";
const BYTES_TEMPLATE: &str =
    "{prefix:>12.green.bold} {bar:24.green/blue} {percent:>3}% {bytes}/{total_bytes} {msg}";
const SPINNER_TEMPLATE: &str = "{prefix:>12.green.bold} {spinner:.green} {msg} ({elapsed})";

static RENDERER: OnceLock<Option<MultiProgress>> = OnceLock::new();

/// Enables or disables process-wide progress rendering.
///
/// Only the first call takes effect, matching the single initialization performed by `main`.
/// Rendering is additionally suppressed whenever stderr is not a terminal, so redirected
/// output never accumulates redraw escape sequences.
pub fn init(enabled: bool) {
    let renderer = (enabled && stderr_is_terminal()).then(MultiProgress::new);
    let _ = RENDERER.set(renderer);
}

/// Runs `operation` with any active progress bars hidden.
///
/// Falls back to running `operation` directly when rendering is disabled or uninitialized,
/// which is what makes this safe to call from the logging writer during early startup.
pub fn suspend<F, R>(operation: F) -> R
where
    F: FnOnce() -> R,
{
    match RENDERER.get() {
        Some(Some(renderer)) => renderer.suspend(operation),
        _ => operation(),
    }
}

/// Reports whether progress bars are being drawn.
#[must_use]
pub fn is_enabled() -> bool {
    matches!(RENDERER.get(), Some(Some(_)))
}

/// Creates the top-level bar tracking a command's ordered phases.
#[must_use]
pub fn phases(prefix: &'static str, total: u64) -> Bar {
    Bar::new(prefix, total, PHASE_TEMPLATE, false)
}

/// Creates a bar measuring discrete units of work such as sessions, files, or directories.
#[must_use]
pub fn counted(prefix: &'static str, total: u64) -> Bar {
    Bar::new(prefix, total, COUNT_TEMPLATE, false)
}

/// Creates a bar measuring byte progress.
#[must_use]
pub fn bytes(prefix: &'static str, total: u64) -> Bar {
    Bar::new(prefix, total, BYTES_TEMPLATE, false)
}

/// Creates an indeterminate spinner for work with no measurable completion signal.
#[must_use]
pub fn spinner(prefix: &'static str, message: &'static str) -> Bar {
    let bar = Bar::new(prefix, 0, SPINNER_TEMPLATE, true);
    bar.set_message(message);
    bar
}

/// A progress handle that renders when enabled and does nothing when it is not.
#[derive(Clone, Debug)]
pub struct Bar {
    inner: Option<ProgressBar>,
}

impl Bar {
    fn new(prefix: &'static str, total: u64, template: &str, steady: bool) -> Self {
        let Some(Some(renderer)) = RENDERER.get() else {
            return Self { inner: None };
        };
        let style = ProgressStyle::with_template(template)
            .expect("progress templates are validated by unit tests")
            .progress_chars("=>-");
        let bar = renderer.add(ProgressBar::new(total));
        bar.set_style(style);
        bar.set_prefix(prefix);
        if steady {
            bar.enable_steady_tick(TICK);
        }
        Self { inner: Some(bar) }
    }

    /// Replaces the trailing description.
    pub fn set_message(&self, message: impl Into<String>) {
        if let Some(bar) = &self.inner {
            bar.set_message(message.into());
        }
    }

    /// Replaces the total amount of work.
    pub fn set_length(&self, total: u64) {
        if let Some(bar) = &self.inner {
            bar.set_length(total);
        }
    }

    /// Advances completed work by `delta`.
    pub fn inc(&self, delta: u64) {
        if let Some(bar) = &self.inner {
            bar.inc(delta);
        }
    }

    /// Sets completed work to an absolute value.
    pub fn set_position(&self, position: u64) {
        if let Some(bar) = &self.inner {
            bar.set_position(position);
        }
    }

    /// Advances one step and describes the step just started.
    pub fn step(&self, message: impl Into<String>) {
        if let Some(bar) = &self.inner {
            bar.inc(1);
            bar.set_message(message.into());
        }
    }

    /// Removes the bar from the terminal, leaving the report as the only output.
    pub fn finish(&self) {
        if let Some(bar) = &self.inner {
            bar.finish_and_clear();
        }
    }
}

impl Drop for Bar {
    fn drop(&mut self) {
        if let Some(bar) = &self.inner {
            bar.finish_and_clear();
        }
    }
}

fn stderr_is_terminal() -> bool {
    use std::io::IsTerminal;

    io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use indicatif::ProgressStyle;

    use super::{
        BYTES_TEMPLATE, Bar, COUNT_TEMPLATE, PHASE_TEMPLATE, SPINNER_TEMPLATE, is_enabled, suspend,
    };

    #[test]
    fn every_template_is_a_valid_progress_style() {
        for template in [
            PHASE_TEMPLATE,
            COUNT_TEMPLATE,
            BYTES_TEMPLATE,
            SPINNER_TEMPLATE,
        ] {
            assert!(
                ProgressStyle::with_template(template).is_ok(),
                "template `{template}` must parse"
            );
        }
    }

    #[test]
    fn suspend_runs_the_operation_even_without_a_renderer() {
        assert_eq!(suspend(|| 7), 7);
    }

    #[test]
    fn a_disabled_bar_accepts_every_operation_without_rendering() {
        // Tests never initialize the renderer, so this exercises the no-op path.
        assert!(!is_enabled());
        let bar = Bar { inner: None };
        bar.set_length(10);
        bar.set_message("working");
        bar.inc(1);
        bar.set_position(5);
        bar.step("next");
        bar.finish();
    }
}
