use std::process::{Command, Output};

fn run_sctx(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(args)
        .output()
        .expect("sctx should start")
}

#[test]
fn help_is_the_only_default_surface() {
    let output = run_sctx(&["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("Usage: sctx [OPTIONS]"));
    assert!(stdout.contains("Commands are intentionally unavailable"));
}

#[test]
fn version_uses_the_workspace_package_version() {
    let output = run_sctx(&["--version"]);

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("sctx {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn subcommands_are_rejected_until_their_work_items_land() {
    let output = run_sctx(&["setup"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(2));
    assert!(stderr.contains("command is not available in the workspace scaffold"));
}
