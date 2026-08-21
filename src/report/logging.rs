use std::io::{self, IsTerminal, Write};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::LogMode;
use crate::error::Error;

/// Resolves the effective logging mode from the flag and the ambient `RUST_LOG`.
///
/// `--log` wins whenever it selects a mode, and a bare `RUST_LOG` implicitly enables
/// text diagnostics so an existing debugging habit keeps working without a second flag.
#[must_use]
pub fn effective_mode(requested: LogMode) -> LogMode {
    if requested.is_enabled() {
        return requested;
    }
    if std::env::var_os("RUST_LOG").is_some() {
        LogMode::Text
    } else {
        LogMode::Off
    }
}

/// Installs stderr diagnostics for the resolved logging mode.
///
/// [`LogMode::Off`] installs no subscriber at all, so tracing macros become no-ops and
/// neither reports nor progress rendering are interleaved with diagnostics.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when a global tracing subscriber is already installed.
pub fn init(mode: LogMode) -> Result<(), Error> {
    let result = match effective_mode(mode) {
        LogMode::Off => return Ok(()),
        LogMode::Text => tracing_subscriber::registry()
            .with(filter())
            .with(
                fmt::layer()
                    .with_ansi(io::stderr().is_terminal())
                    .with_writer(|| ProgressAwareStderr),
            )
            .try_init(),
        LogMode::Json => tracing_subscriber::registry()
            .with(filter())
            .with(
                fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(|| ProgressAwareStderr),
            )
            .try_init(),
    };

    result.map_err(|source| Error::InvalidArgument {
        argument: "--log".to_owned(),
        reason: format!("failed to initialize tracing subscriber: {source}"),
    })
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// A stderr writer that pauses active progress bars for the duration of one write.
///
/// Without this, a diagnostic line and a redrawing progress bar race for the same
/// terminal row and produce exactly the interleaved output `--log` exists to avoid.
struct ProgressAwareStderr;

impl Write for ProgressAwareStderr {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        super::progress::suspend(|| io::stderr().write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{LogMode, effective_mode};

    #[test]
    fn an_explicit_mode_is_never_overridden() {
        assert_eq!(effective_mode(LogMode::Text), LogMode::Text);
        assert_eq!(effective_mode(LogMode::Json), LogMode::Json);
    }
}
