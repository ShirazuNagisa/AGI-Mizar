//! The UI-facing installer commands, with a fake `agi-remote` on disk.

use std::os::unix::fs::PermissionsExt;

use agi_core::Core;
use agi_protocol::api::Command;

#[tokio::test]
async fn the_kernel_runs_the_installer_without_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("agi-remote");
    std::fs::write(&fake, "#!/bin/sh\nprintf 'installed %s\\n' \"$1\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::env::set_var("AGI_REMOTE_BIN", &fake);

    let core = Core::new(Vec::new());
    let response = core
        .dispatch(Command::RemoteInstall {
            profile: Some("desktop".to_string()),
            dry_run: true,
        })
        .await;
    assert!(response.ok, "{response:?}");
    let value = response.value.unwrap();
    assert!(value["stdout"]
        .as_str()
        .unwrap()
        .contains("installed install"));

    let missing = fake.with_file_name("nope");
    std::env::set_var("AGI_REMOTE_BIN", &missing);
    let missing = core
        .dispatch(Command::RemoteInstall {
            profile: None,
            dry_run: false,
        })
        .await;
    assert!(!missing.ok);
    assert_eq!(missing.error.unwrap().code, "REMOTE_INSTALLER_MISSING");
}
