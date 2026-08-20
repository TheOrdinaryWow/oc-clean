use std::io;

use tracing_indicatif::IndicatifLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::cli::LogFormat;
use crate::error::Error;

/// Installs stderr diagnostics and optional stderr progress rendering.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] when a global tracing subscriber is already installed.
pub fn init(format: LogFormat, progress: bool) -> Result<(), Error> {
    let result = match (format, progress) {
        (LogFormat::Text, true) => {
            let indicatif = IndicatifLayer::new();
            let writer = indicatif.get_stderr_writer();
            tracing_subscriber::registry()
                .with(filter())
                .with(fmt::layer().with_writer(writer))
                .with(indicatif)
                .try_init()
        }
        (LogFormat::Json, true) => {
            let indicatif = IndicatifLayer::new();
            let writer = indicatif.get_stderr_writer();
            tracing_subscriber::registry()
                .with(filter())
                .with(fmt::layer().json().with_ansi(false).with_writer(writer))
                .with(indicatif)
                .try_init()
        }
        (LogFormat::Text, false) => tracing_subscriber::registry()
            .with(filter())
            .with(fmt::layer().with_writer(io::stderr))
            .try_init(),
        (LogFormat::Json, false) => tracing_subscriber::registry()
            .with(filter())
            .with(fmt::layer().json().with_ansi(false).with_writer(io::stderr))
            .try_init(),
    };

    result.map_err(|source| Error::InvalidArgument {
        argument: "--log-format".to_owned(),
        reason: format!("failed to initialize tracing subscriber: {source}"),
    })
}

fn filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}
