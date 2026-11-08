use std::io::{ErrorKind, Write};
use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = shaphansaudit::cli::Cli::parse();
    let out = match shaphansaudit::run(&cli) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("shaphan: {e}");
            return ExitCode::FAILURE;
        }
    };
    match std::io::stdout().lock().write_all(out.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shaphan: cannot write output: {e}");
            ExitCode::FAILURE
        }
    }
}
