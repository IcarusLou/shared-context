use std::process::{Command, Output};
use tempfile::tempdir;

fn run_sctx(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sctx"))
        .args(args)
        .output()
        .expect("sctx should start")
}

#[test]
fn help_exposes_only_the_landed_workspace_surface() {
    let output = run_sctx(&["--help"]);
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success());
    assert!(stdout.contains("Usage: sctx [OPTIONS]"));
    assert!(stdout.contains("workspace <COMMAND>"));
    assert!(stdout.contains("bind --workspace <PATH> --space-id <SPACE_ID>"));
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
    assert!(stderr.contains("command is not available in this implementation stage"));
}

#[test]
fn workspace_bind_list_unbind_support_spaces_and_chinese_paths() {
    let temporary = tempdir().unwrap();
    let home = temporary.path().join("用户 home 空格");
    let workspace = temporary.path().join("业务 workspace 中文");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let space_id = "spc_aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let run = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_sctx"))
            .args(args)
            .env("HOME", &home)
            .output()
            .expect("sctx should start")
    };

    let bind = run(&[
        std::ffi::OsStr::new("workspace"),
        std::ffi::OsStr::new("bind"),
        std::ffi::OsStr::new("--workspace"),
        workspace.as_os_str(),
        std::ffi::OsStr::new("--space-id"),
        std::ffi::OsStr::new(space_id),
    ]);
    assert!(
        bind.status.success(),
        "{}",
        String::from_utf8_lossy(&bind.stderr)
    );

    let list = run(&[
        std::ffi::OsStr::new("workspace"),
        std::ffi::OsStr::new("list"),
    ]);
    let listed = String::from_utf8_lossy(&list.stdout);
    assert!(list.status.success());
    assert!(listed.contains(workspace.to_str().unwrap()));
    assert!(listed.contains(space_id));

    let unbind = run(&[
        std::ffi::OsStr::new("workspace"),
        std::ffi::OsStr::new("unbind"),
        std::ffi::OsStr::new("--workspace"),
        workspace.as_os_str(),
    ]);
    assert!(unbind.status.success());

    let empty = run(&[
        std::ffi::OsStr::new("workspace"),
        std::ffi::OsStr::new("list"),
    ]);
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
}
