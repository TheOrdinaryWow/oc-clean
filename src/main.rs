use clap::Parser;
use oc_clean::{
    cli::{Cli, Commands},
    doctor,
    error::Error,
    report,
};

fn main() {
    let cli = Cli::parse();
    let result = initialize_logging(&cli).and_then(|()| dispatch(&cli));
    handle_result(result);
}

fn initialize_logging(cli: &Cli) -> Result<(), Error> {
    match &cli.command {
        Commands::Analyze(arguments) => {
            report::logging::init(arguments.log_format, !arguments.json)
        }
        Commands::Doctor(_) => report::logging::init(oc_clean::cli::LogFormat::Text, false),
    }
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
    }
}

fn handle_result(result: Result<(), Error>) {
    if let Err(error) = result {
        tracing::error!(exit_code = error.exit_code(), error = %error, "command failed");
        std::process::exit(error.exit_code());
    }
}
