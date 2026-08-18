//! `sctx` command-line entry point.

use std::{env, ffi::OsString, path::PathBuf, process::ExitCode, str::FromStr};

use sctx_domain::{Error, ErrorKind, Result, SpaceId};
use sctx_local_state::UserConfigStore;

const HELP: &str = "Shared Context command-line interface\n\n\
Usage: sctx [OPTIONS]\n\
       sctx workspace <COMMAND>\n\n\
Options:\n  \
  -h, --help     Print help\n  \
  -V, --version  Print version\n\n\
Workspace commands:\n  \
  bind --workspace <PATH> --space-id <SPACE_ID>  Set a local query hint\n  \
  list                                             List query hints\n  \
  unbind --workspace <PATH>                        Remove a query hint\n\n\
Other commands are intentionally unavailable in this implementation stage.\n";

fn main() -> ExitCode {
    let mut process_args = env::args_os();
    let _program = process_args.next();
    let args = process_args.collect::<Vec<_>>();

    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(args: &[OsString]) -> Result<()> {
    match args {
        [] => {
            print!("{HELP}");
            Ok(())
        }
        [arg] if arg == "-h" || arg == "--help" => {
            print!("{HELP}");
            Ok(())
        }
        [arg] if arg == "-V" || arg == "--version" => {
            println!("sctx {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        [workspace, rest @ ..] if workspace == "workspace" => run_workspace(rest),
        _ => Err(Error::new(
            ErrorKind::Unsupported,
            format!("command is not available in this implementation stage\n\n{HELP}"),
        )),
    }
}

fn run_workspace(args: &[OsString]) -> Result<()> {
    let config = UserConfigStore::initialize(installation_root()?)?;
    match args {
        [command, workspace_flag, workspace, space_flag, space]
            if command == "bind"
                && workspace_flag == "--workspace"
                && space_flag == "--space-id" =>
        {
            let space = text(space, "space ID")?;
            let space_id = SpaceId::from_str(space).map_err(|error| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid space ID: {error}"),
                )
            })?;
            let binding = config.bind(PathBuf::from(workspace), space_id)?;
            println!(
                "bound\t{}\t{}",
                binding.workspace().display(),
                binding.space_id()
            );
            Ok(())
        }
        [command] if command == "list" => {
            for binding in config.list()? {
                println!("{}\t{}", binding.workspace().display(), binding.space_id());
            }
            Ok(())
        }
        [command, workspace_flag, workspace]
            if command == "unbind" && workspace_flag == "--workspace" =>
        {
            match config.unbind(PathBuf::from(workspace))? {
                Some(binding) => println!(
                    "unbound\t{}\t{}",
                    binding.workspace().display(),
                    binding.space_id()
                ),
                None => println!("not-bound\t{}", PathBuf::from(workspace).display()),
            }
            Ok(())
        }
        _ => Err(Error::new(
            ErrorKind::InvalidInput,
            format!("invalid workspace command\n\n{HELP}"),
        )),
    }
}

fn installation_root() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".shared-context"))
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "HOME is not set"))
}

fn text<'a>(value: &'a OsString, field: &str) -> Result<&'a str> {
    value.to_str().ok_or_else(|| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("{field} is not valid UTF-8"),
        )
    })
}
