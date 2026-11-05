//! One install, end to end, without touching the machine's real DSH.
//!
//! The command is the part that is hard to unit test: it resolves a profile,
//! passes `DSH_HOME` to a child process, waits for pnpm, and reads the version
//! back from disk. A fake `dsh` shell script stands in for the real launcher and
//! writes the same two files DSH's plugin command writes.
//!
//! One test function rather than two: both cases set process-wide environment
//! variables, and a parallel harness would let one case's `AGI_DSH_CLI` leak into
//! the other.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn an_install_writes_through_the_cli_and_reads_the_version_back() {
    let home = tempfile::tempdir().unwrap();
    let profile = home.path().join("profiles").join("desktop");
    std::fs::create_dir_all(&profile).unwrap();

    let cli = home.path().join("fake-dsh");
    std::fs::write(
        &cli,
        r#"#!/bin/sh
set -eu
profile="desktop"
last=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --profile) shift; profile="$1" ;;
    *) last="$1" ;;
  esac
  shift
done
version="${last#dsh-mizar@}"
dir="$DSH_HOME/profiles/$profile"
mkdir -p "$dir/node_modules/dsh-mizar"
printf '{"name":"dsh-mizar","version":"%s"}\n' "$version" > "$dir/node_modules/dsh-mizar/package.json"
printf '{"dsh":{"profile":{"bundles":["dsh-mizar"]}}}\n' > "$dir/package.json"
echo "fake pnpm: added dsh-mizar@$version"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&cli).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&cli, permissions).unwrap();

    std::env::set_var("DSH_HOME", home.path());
    std::env::set_var("AGI_DSH_CLI", &cli);

    let installed = agi_core::plugin::install("0.9.3").await.unwrap();
    assert_eq!(installed.version.as_deref(), Some("0.9.3"));
    assert!(agi_core::plugin::bundle_listed(&profile));
    assert!(
        installed.output.contains("0.9.3"),
        "the package manager's words are kept: {}",
        installed.output
    );

    // The override is authoritative: a moved launcher must not silently fall
    // back to another DSH and install into the wrong tree.
    std::env::set_var("AGI_DSH_CLI", home.path().join("moved-dsh"));
    let failure = agi_core::plugin::install("0.9.3").await.unwrap_err();
    assert!(
        failure.contains("dsh 命令行"),
        "a missing launcher has its own sentence: {failure}"
    );
}
