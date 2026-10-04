use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;

pub(crate) fn write(path: &Path, script: &str) {
    let status = Command::new("sh")
        .args(["-c", "printf %s \"$1\" > \"$2\"", "sh", script])
        .arg(path)
        .status()
        .unwrap_or_else(|error| panic!("cannot start sh to write {}: {error}", path.display()));
    assert!(
        status.success(),
        "sh could not write {}: {status}",
        path.display()
    );
    fs::set_permissions(path, Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("cannot make {} executable: {error}", path.display()));
}
