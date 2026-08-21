use std::io::{self, Write};

use clap::Parser;
use oc_clean::{
    cli::{Cli, Commands},
    doctor,
    error::Error,
    report,
};

fn main() {
    let cli = Cli::parse();
    let json = cli.json();
    let result = initialize(&cli).and_then(|()| dispatch(&cli));
    handle_result(result, json);
}

/// Installs diagnostics and progress rendering before any command runs.
///
/// Progress is drawn on stderr, so it is suppressed whenever the command emits a JSON
/// report; that keeps a piped `--json` run byte-clean on both streams.
fn initialize(cli: &Cli) -> Result<(), Error> {
    report::logging::init(cli.log)?;
    report::progress::init(!cli.json());
    Ok(())
}

fn dispatch(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Commands::Analyze(arguments) => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            report::command::run(cli, arguments, &mut output)
        }
        Commands::Doctor(arguments) => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            doctor::command::run(cli, arguments, &mut output)
        }
        Commands::Clean(arguments) => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            oc_clean::clean::command::run(cli, arguments, &mut output)
        }
        Commands::Vacuum(arguments) => {
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            oc_clean::reclaim::command::run(cli, arguments, &mut output)
        }
    }
}

/// Renders a failure on stderr in the requested format and exits with its stable code.
///
/// Failures never depend on `--log` being enabled, because diagnostics are off by default
/// and a silent non-zero exit would leave an operator with no way to learn what went wrong.
/// They also never reach stdout: a command such as `doctor --json` writes its report before
/// returning a failure, and stdout must keep carrying exactly one report object.
fn handle_result(result: Result<(), Error>, json: bool) {
    let Err(error) = result else {
        return;
    };
    tracing::error!(exit_code = error.exit_code(), error = %error, "command failed");
    let stderr = io::stderr();
    let mut output = stderr.lock();
    let rendered = if json {
        report::failure::write_json(&error, &mut output)
    } else {
        report::failure::write_human(&error, &mut output)
    }
    .and_then(|()| output.flush());
    // A broken pipe on the diagnostic stream must not mask the original failure's exit code.
    drop(rendered);
    std::process::exit(error.exit_code());
}
