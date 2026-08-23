use std::{collections::HashMap, fs, path::PathBuf, process::Command};

const MEMBERS: [(&str, u8); 16] = [
    ("scenario-contract", 0),
    ("scenario-runner", 2),
    ("domain", 0),
    ("engineering-graph", 1),
    ("local-state", 1),
    ("event-schema", 1),
    ("task-runtime", 1),
    ("git-store", 2),
    ("index", 3),
    ("search", 4),
    ("mcp", 5),
    ("agent-adapter", 5),
    ("adapter-cursor", 6),
    ("adapter-codex", 6),
    ("installer", 7),
    ("cli", 8),
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn required_workspace_members_have_manifests() {
    let root = workspace_root();

    for (member, _) in MEMBERS {
        let manifest = root.join("crates").join(member).join("Cargo.toml");
        assert!(manifest.is_file(), "missing {}", manifest.display());
    }
}

#[test]
fn workspace_dependencies_only_point_to_lower_layers() {
    let root = workspace_root();
    let ranks = MEMBERS.into_iter().collect::<HashMap<_, _>>();

    for (member, member_rank) in MEMBERS {
        let manifest_path = root.join("crates").join(member).join("Cargo.toml");
        let manifest =
            fs::read_to_string(&manifest_path).expect("member manifest should be readable");
        let mut in_dependencies = false;

        for line in manifest.lines().map(str::trim) {
            if line == "[dependencies]" {
                in_dependencies = true;
                continue;
            }
            if line.starts_with('[') {
                in_dependencies = false;
            }
            if !in_dependencies || !line.starts_with("sctx-") {
                continue;
            }

            let dependency = line
                .split_once('.')
                .map(|(name, _)| name)
                .expect("workspace dependency should use `<name>.workspace`")
                .strip_prefix("sctx-")
                .expect("workspace dependency should use the sctx prefix");
            let dependency_rank = ranks
                .get(dependency)
                .unwrap_or_else(|| panic!("unknown workspace dependency: {dependency}"));

            assert!(
                *dependency_rank < member_rank,
                "{member} (layer {member_rank}) must not depend on {dependency} (layer {dependency_rank})"
            );
        }
    }
}

#[test]
fn source_asset_trees_are_not_ignored() {
    let root = workspace_root();

    for path in [
        "schemas/README.md",
        "fixtures/README.md",
        "fixtures/agents/cursor-3.13.json",
        "fixtures/agents/codex-0.147.json",
        "npm/packages/README.md",
        "docs/dynamic-replay-phase-one.md",
        "tests/reports/dynamic-replay-phase-one-v1.json",
    ] {
        let status = Command::new("git")
            .args(["check-ignore", "--quiet", "--no-index", path])
            .current_dir(&root)
            .status()
            .expect("git check-ignore should run");

        assert_eq!(
            status.code(),
            Some(1),
            "{path} should remain trackable by Git"
        );
    }
}
