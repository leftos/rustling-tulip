//! Windows file helpers the refresh and store test binaries share: handles that keep a file from being read, renamed
//! or removed (Windows only), and the temp files a write leaves beside its target.

use std::fs;
#[cfg(windows)]
use std::fs::{File, OpenOptions};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::thread::{self, JoinHandle};
#[cfg(windows)]
use std::time::{Duration, Instant};

/// `FILE_SHARE_READ` and `FILE_SHARE_WRITE`: a handle that lets others read and write the file, but not rename or
/// delete it.
#[cfg(windows)]
const FILE_SHARE_READ: u32 = 0x1;
#[cfg(windows)]
const FILE_SHARE_WRITE: u32 = 0x2;

/// The names of the files in `folder` that end in `.tmp`.
pub fn temp_files(folder: &Path) -> Vec<String> {
    fs::read_dir(folder)
        .expect("the folder can be listed")
        .map(|entry| {
            entry
                .expect("an entry can be read")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("tmp"))
        })
        .collect()
}

/// Opens the first temp file to appear in `folder` without delete sharing, so it can be neither renamed nor removed
/// while the returned handle lives; gives up after 5 s.
#[cfg(windows)]
pub fn pin_first_temp(folder: PathBuf) -> JoinHandle<File> {
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let pinned = temp_files(&folder).first().and_then(|name| {
                OpenOptions::new()
                    .read(true)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                    .open(folder.join(name))
                    .ok()
            });
            if let Some(file) = pinned {
                return file;
            }
            assert!(Instant::now() < deadline, "no temp file appeared");
            thread::sleep(Duration::from_millis(1));
        }
    })
}

/// Opens `path` with no sharing, so nothing can read, replace or delete it until the handle drops.
#[cfg(windows)]
pub fn hold_open(path: &Path) -> File {
    OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .expect("the file opens with no sharing")
}
