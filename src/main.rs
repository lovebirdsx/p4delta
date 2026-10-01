use clap::Parser;
use p4delta::Options;
use std::process::ExitCode;

fn main() -> ExitCode {
    let options = Options::parse();

    match p4delta::run(options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}
