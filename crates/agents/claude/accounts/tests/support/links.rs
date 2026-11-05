//! Link helpers the refresh and store test binaries share: a junction, which needs no privilege, a file symlink, which
//! needs Developer Mode or elevation and is skipped without them, and a snapshot of the folder a link points at.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Every file in `folder` by name, with its bytes: what a planted folder held, to compare after a write.
pub fn snapshot(folder: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(folder)
        .expect("the folder lists")
        .map(|entry| {
            let entry = entry.expect("an entry can be read");
            let bytes = fs::read(entry.path()).expect("a planted file can be read");
            (entry.file_name().to_string_lossy().into_owned(), bytes)
        })
        .collect()
}

/// Makes `link` a junction to the folder `target`, which needs no privilege; off Windows a symbolic link to it.
#[cfg(unix)]
pub fn junction(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).expect("the folder symlink can be made");
}

/// Makes `link` a junction to the folder `target`, which needs no privilege.
#[cfg(windows)]
pub fn junction(link: &Path, target: &Path) {
    let made = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("cmd can be started");
    assert!(
        made.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&made.stderr)
    );
}

/// Makes `link` a file symlink to `target`: false when this machine does not allow it (no Developer Mode and no
/// elevation, os error 1314), so the caller skips. Off Windows a symlink needs no privilege and is always made.
#[cfg(unix)]
pub fn symlink_or_skip(link: &Path, target: &Path) -> bool {
    std::os::unix::fs::symlink(target, link).expect("the symlink can be made");
    true
}

/// Makes `link` a file symlink to `target`: false when this machine does not allow it (no Developer Mode and no
/// elevation, os error 1314), so the caller skips.
#[cfg(windows)]
#[expect(
    clippy::print_stdout,
    reason = "a test that cannot make its symlink says why it was skipped"
)]
pub fn symlink_or_skip(link: &Path, target: &Path) -> bool {
    let made = std::os::windows::fs::symlink_file(target, link);
    if made.as_ref().err().and_then(std::io::Error::raw_os_error) == Some(1314) {
        println!("skipped: creating a symlink needs Developer Mode or elevation");
        return false;
    }
    made.expect("the symlink can be made");
    true
}
