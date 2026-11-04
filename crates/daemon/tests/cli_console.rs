//! The daemon's CLI verbs write to a redirected stdout even when the caller
//! holds a console: `ssh host cmd /c "rustling-tulipd --version > out.txt"`
//! runs cmd with a hidden console that the GUI-subsystem daemon attaches to.

#![cfg(windows)]

mod cli_pair {
    #[expect(
        clippy::expect_used,
        reason = "tests assert spawn and file preconditions with expect for clear failure messages"
    )]
    mod tests {
        use std::os::windows::io::AsRawHandle as _;
        use std::os::windows::process::CommandExt as _;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::WaitForSingleObject;

        /// Only a hang guard, in milliseconds: the test waits on cmd's exit
        /// and asserts nothing about how long it took.
        const EXIT_HANG_GUARD_MS: u32 = 60_000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        /// A fresh folder under the repo's `.tmp/`, removed on drop.
        struct TestDir(PathBuf);

        impl TestDir {
            fn new(tag: &str) -> Self {
                let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("..")
                    .join("..")
                    .join(".tmp")
                    .join(format!("cli-console-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("create the test dir");
                Self(path)
            }
        }

        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn redirected_version_reaches_the_file() {
            let dir = TestDir::new("version");
            let out = dir.0.join("out.txt");
            let exe = env!("CARGO_BIN_EXE_rustling-tulipd");
            // cmd strips the outer quotes of a `/c` line that starts with one.
            // `cmd /c` waits for a GUI-subsystem program it runs (it exits with
            // that program's exit code), so cmd's exit is the daemon's.
            let line = format!("/c \"\"{exe}\" --version > \"{}\"\"", out.display());
            let mut child = Command::new(r"C:\Windows\System32\cmd.exe")
                .raw_arg(line)
                .env("RUSTLING_TULIP_CONFIG_DIR", dir.0.join("config"))
                .env("RUSTLING_TULIP_BINARIES_DIR", dir.0.join("binaries"))
                .env("RUSTLING_TULIP_WORKTREES_DIR", dir.0.join("worktrees"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // A hidden console of cmd's own, as sshd gives it.
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .expect("start cmd");
            // SAFETY: the handle is `child`'s own process handle, live while
            // `child` is; the wait only reads it.
            let waited =
                unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), EXIT_HANG_GUARD_MS) };
            if waited != WAIT_OBJECT_0 {
                let _ = child.kill();
            }
            assert_eq!(
                waited, WAIT_OBJECT_0,
                "hung: cmd and the daemon it waits for had not exited"
            );
            let status = child.wait().expect("read cmd's exit status");
            assert!(status.success(), "cmd ran the redirect: {status}");

            let expected = format!("rustling-tulipd {}", env!("CARGO_PKG_VERSION"));
            let text = std::fs::read_to_string(&out).expect("read the redirected file");
            assert_eq!(
                text.trim_end(),
                expected,
                "the redirected file holds the version line"
            );
        }
    }
}
