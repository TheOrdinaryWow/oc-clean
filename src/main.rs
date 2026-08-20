use clap::Parser;
use oc_clean::{cli::Cli, error::Error};

fn main() {
    let _cli = Cli::parse();
    let result: Result<(), Error> = Ok(());
    handle_result(result);
}

fn handle_result(result: Result<(), Error>) {
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(error.exit_code());
    }
}
