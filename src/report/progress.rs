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

const TICK: Duration = Duration::from_millis(100);

/// Bar fill characters: filled, leading edge, remaining.
const PROGRESS_CHARS: &str = "█▓░";

/// Braille frames for indeterminate work.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠿"];

// Phase bars deliberately carry no ETA. Phases measure heterogeneous work — a schema
// inspection and a full-table delete are both "one phase" — so a rate extrapolated from
// completed phases predicts the remaining ones badly enough to be worse than no estimate.
const PHASE_TEMPLATE: &str =
    "{prefix:>12.cyan.bold} [{pos:>2}/{len:<2}] {wide_bar:.cyan/blue} {msg}";
const COUNT_TEMPLATE: &str = "{prefix:>12.green.bold} {wide_bar:.green/blue} {percent:>3}% \
     {pos}/{len} {per_sec} eta {eta} {msg}";
const BYTES_TEMPLATE: &str = "{prefix:>12.green.bold} {wide_bar:.green/blue} {percent:>3}% \
     {bytes}/{total_bytes} {binary_bytes_per_sec} eta {eta} {msg}";
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
    Bar::new(prefix, total, PHASE_TEMPLATE, Motion::Driven)
}

/// Creates a bar measuring discrete units of work such as sessions, files, or directories.
#[must_use]
pub fn counted(prefix: &'static str, total: u64) -> Bar {
    Bar::new(prefix, total, COUNT_TEMPLATE, Motion::Driven)
}

/// Creates a bar measuring byte progress.
#[must_use]
pub fn bytes(prefix: &'static str, total: u64) -> Bar {
    Bar::new(prefix, total, BYTES_TEMPLATE, Motion::Driven)
}

/// Creates an indeterminate spinner for work with no measurable completion signal.
#[must_use]
pub fn spinner(prefix: &'static str, message: impl Into<String>) -> Bar {
    let bar = Bar::new(prefix, 0, SPINNER_TEMPLATE, Motion::Animated);
    bar.set_message(message.into());
    bar
}

/// Whether a bar advances only when the caller reports progress, or animates on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Motion {
    /// The caller drives every redraw by reporting completed work.
    Driven,
    /// A background ticker animates the bar while the caller is blocked.
    Animated,
}

/// A progress handle that renders when enabled and does nothing when it is not.
#[derive(Clone, Debug)]
pub struct Bar {
    inner: Option<ProgressBar>,
}

impl Bar {
    fn new(prefix: &'static str, total: u64, template: &str, motion: Motion) -> Self {
        let Some(Some(renderer)) = RENDERER.get() else {
            return Self { inner: None };
        };
        let bar = renderer.add(ProgressBar::new(total));
        bar.set_style(style(template));
        bar.set_prefix(prefix);
        if motion == Motion::Animated {
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

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("progress templates are validated by unit tests")
        .progress_chars(PROGRESS_CHARS)
        .tick_strings(SPINNER_FRAMES)
}

fn stderr_is_terminal() -> bool {
    use std::io::IsTerminal;

    io::stderr().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::{
        BYTES_TEMPLATE, Bar, COUNT_TEMPLATE, PHASE_TEMPLATE, PROGRESS_CHARS, SPINNER_FRAMES,
        SPINNER_TEMPLATE, is_enabled, style, suspend,
    };

    const TEMPLATES: [&str; 4] = [
        PHASE_TEMPLATE,
        COUNT_TEMPLATE,
        BYTES_TEMPLATE,
        SPINNER_TEMPLATE,
    ];

    #[test]
    fn every_template_builds_a_style_with_the_shared_decorations() {
        // `style` panics on an invalid template or an under-length character set, so a
        // successful call over every template is the assertion.
        for template in TEMPLATES {
            let _ = style(template);
        }
    }

    #[test]
    fn bar_characters_cover_filled_edge_and_remaining() {
        assert_eq!(PROGRESS_CHARS.chars().count(), 3);
        assert!(SPINNER_FRAMES.len() >= 2, "indicatif requires two frames");
    }

    #[test]
    fn only_measurable_work_advertises_a_rate_and_an_estimate() {
        for template in [COUNT_TEMPLATE, BYTES_TEMPLATE] {
            assert!(
                template.contains("{eta}"),
                "`{template}` should show an ETA"
            );
        }
        assert!(COUNT_TEMPLATE.contains("{per_sec}"));
        assert!(BYTES_TEMPLATE.contains("{binary_bytes_per_sec}"));
        // Phases measure heterogeneous work, so an extrapolated estimate would mislead.
        assert!(!PHASE_TEMPLATE.contains("{eta}"));
        assert!(!PHASE_TEMPLATE.contains("{per_sec}"));
    }

    #[test]
    fn every_bar_adapts_to_the_terminal_width() {
        for template in [PHASE_TEMPLATE, COUNT_TEMPLATE, BYTES_TEMPLATE] {
            assert!(
                template.contains("{wide_bar"),
                "`{template}` should fill the available width"
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
