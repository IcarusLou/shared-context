//! Placeholder `sctx` command-line entry point.

use std::{env, process::ExitCode};

use sctx_domain::{Error, ErrorKind};

const HELP: &str = "Shared Context command-line interface (workspace skeleton)\n\n\
Usage: sctx [OPTIONS]\n\n\
Options:\n  \
  -h, --help     Print help\n  \
  -V, --version  Print version\n\n\
Commands are intentionally unavailable in this scaffold.\n";

fn main() -> ExitCode {
    let mut process_args = env::args_os();
    let _program = process_args.next();
    let args = process_args.collect::<Vec<_>>();

    match args.as_slice() {
        [] => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [arg] if arg == "-h" || arg == "--help" => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        [arg] if arg == "-V" || arg == "--version" => {
            println!("sctx {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => {
            let error = Error::new(
                ErrorKind::Unsupported,
                "command is not available in the workspace scaffold",
            );
            eprintln!("error: {error}\n\n{HELP}");
            ExitCode::from(2)
        }
    }
}
