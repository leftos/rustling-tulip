//! Opening a web page's `file:` URL, `#fragment` and `?query` included, in
//! the default browser off Windows: on macOS through the app registered for
//! `https`, elsewhere through the `Exec` line of the XDG default browser's
//! desktop entry. The URL only ever travels as one argument; no shell runs.

#[cfg(any(all(unix, not(target_os = "macos")), test))]
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;

/// The JavaScript for Automation script `osascript` runs: it finds the app
/// macOS opens `https` links with and hands it the URL, which arrives as the
/// script's first argument and is never part of the script's text. It throws
/// when no app opens `https` links, so `osascript` exits non-zero.
#[cfg(any(target_os = "macos", test))]
const SCRIPT: &str = "ObjC.import('AppKit');
function run(argv) {
    const appURL = $.NSWorkspace.sharedWorkspace.URLForApplicationToOpenURL(
        $.NSURL.URLWithString('https://example.com'));
    if (!appURL || appURL.isNil()) {
        throw new Error('no app opens https links');
    }
    const app = Application(appURL.path.js);
    app.includeStandardAdditions = true;
    app.openLocation(argv[0]);
}";

/// Opens `url` in the app macOS opens `https` links with, waiting for
/// `osascript` so a missing app or a failed script is an error.
///
/// # Errors
/// When `osascript` would not start or exited non-zero; its stderr says why.
#[cfg(target_os = "macos")]
pub(super) fn open_in_browser(url: &str) -> Result<(), String> {
    let output = Command::new("osascript")
        .arg("-l")
        .arg("JavaScript")
        .arg("-e")
        .arg(SCRIPT)
        .arg(url)
        .output()
        .map_err(|err| format!("start osascript: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("osascript {}: {}", output.status, stderr.trim()))
    }
}

/// Opens `url` with the command line of the XDG default web browser's
/// desktop entry.
///
/// # Errors
/// When the default browser, its desktop entry or its `Exec` line cannot be
/// found or read, or its program would not start.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn open_in_browser(url: &str) -> Result<(), String> {
    let exec = default_browser_exec()?;
    let argv = desktop_exec_argv(&exec, url).ok_or_else(|| format!("no command in Exec={exec}"))?;
    if forwards_files(&argv) {
        return Err(format!(
            "Exec={exec} forwards the page through flatpak's document portal, which drops its suffix"
        ));
    }
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| format!("no command in Exec={exec}"))?;
    Command::new(program)
        .args(args)
        .spawn()
        .map(drop)
        .map_err(|err| format!("start {program}: {err}"))
}

/// The `Exec` line of the desktop entry `xdg-settings` names as the default
/// web browser, found in the XDG application folders.
#[cfg(all(unix, not(target_os = "macos")))]
fn default_browser_exec() -> Result<String, String> {
    let output = Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .output()
        .map_err(|err| format!("start xdg-settings: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("xdg-settings {}: {}", output.status, stderr.trim()));
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    // A desktop file id is a bare file name; anything else is not looked up.
    if id.is_empty() || id.contains('/') || id == ".." {
        return Err(format!("no usable default web browser: {id:?}"));
    }
    let var = |name: &str| std::env::var(name).ok();
    let dirs = xdg_application_dirs(
        var("XDG_DATA_HOME").as_deref(),
        var("HOME").as_deref(),
        var("XDG_DATA_DIRS").as_deref(),
    );
    let entry = dirs
        .iter()
        .map(|dir| dir.join(&id))
        .find(|entry| entry.is_file())
        .ok_or_else(|| format!("no desktop entry {id} in {dirs:?}"))?;
    let contents = std::fs::read_to_string(&entry)
        .map_err(|err| format!("read {}: {err}", entry.display()))?;
    browser_exec(&contents).map_err(|err| format!("{}: {err}", entry.display()))
}

/// The `Exec` line of the browser's desktop entry `contents`.
///
/// # Errors
/// When it has none, or says the browser runs in a terminal, where no
/// window of its own would show the page.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn browser_exec(contents: &str) -> Result<String, String> {
    if desktop_entry_value(contents, "Terminal").as_deref() == Some("true") {
        return Err("the browser runs in a terminal".to_owned());
    }
    desktop_entry_exec(contents).ok_or_else(|| "no Exec line".to_owned())
}

/// Whether the browser command `argv` hands its files to flatpak's file
/// forwarding (`@@u … @@` or `@@ … @@`), which passes a `file:` URL through
/// the document portal without its `#fragment` or `?query`.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn forwards_files(argv: &[String]) -> bool {
    argv.iter().any(|arg| arg == "@@u" || arg == "@@")
}

/// The folders a desktop entry is looked for in, in order: `data_home`
/// (`$XDG_DATA_HOME`, else `home`'s `.local/share`), then each entry of
/// `data_dirs` (`$XDG_DATA_DIRS`, else `/usr/local/share:/usr/share`), each
/// with `/applications` after it. An empty variable counts as unset, and an
/// empty or relative entry is skipped, as the XDG Base Directory spec says.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn xdg_application_dirs(
    data_home: Option<&str>,
    home: Option<&str>,
    data_dirs: Option<&str>,
) -> Vec<PathBuf> {
    let absolute = |dir: &&str| dir.starts_with('/');
    let data_home = match data_home.filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(dir.to_owned()),
        None => home
            .filter(absolute)
            .map(|home| format!("{}/.local/share", home.trim_end_matches('/'))),
    };
    let data_dirs = data_dirs
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or("/usr/local/share:/usr/share");
    data_home
        .into_iter()
        .chain(data_dirs.split(':').map(str::to_owned))
        .filter(|dir| absolute(&dir.as_str()))
        .map(|dir| PathBuf::from(format!("{}/applications", dir.trim_end_matches('/'))))
        .collect()
}

/// The `Exec` value of the `[Desktop Entry]` group of the desktop entry
/// `contents`, trimmed; the `Exec` lines of other groups, such as a
/// `[Desktop Action …]`, are not it.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn desktop_entry_exec(contents: &str) -> Option<String> {
    desktop_entry_value(contents, "Exec")
}

/// The value of `key` in the `[Desktop Entry]` group of the desktop entry
/// `contents`, trimmed; the same key in another group is not it.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn desktop_entry_value(contents: &str, wanted: &str) -> Option<String> {
    let mut in_main_group = false;
    for line in contents.lines().map(str::trim) {
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
        } else if in_main_group
            && let Some((key, value)) = line.split_once('=')
            && key.trim_end() == wanted
        {
            return Some(value.trim().to_owned());
        }
    }
    None
}

/// The URL field codes of a desktop entry's `Exec` line.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
const URL_CODES: [&str; 4] = ["%u", "%U", "%f", "%F"];

/// The other field codes, which a URL launch drops.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
const DROPPED_CODES: [&str; 9] = ["%i", "%c", "%k", "%d", "%D", "%n", "%N", "%v", "%m"];

/// The program and arguments of the desktop entry `Exec` line `exec` with
/// `url` in place of its first `%u`, `%U`, `%f` or `%F`, or after the rest
/// when it has none; later URL codes and the other field codes are dropped
/// and `%%` becomes `%`. `None` when it names no program or does not parse.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn desktop_exec_argv(exec: &str, url: &str) -> Option<Vec<String>> {
    let words = exec_words(exec)?;
    let program = words.first()?;
    if URL_CODES.contains(&program.as_str()) || DROPPED_CODES.contains(&program.as_str()) {
        return None;
    }
    let mut argv = Vec::with_capacity(words.len() + 1);
    let mut placed = false;
    for word in &words {
        if URL_CODES.contains(&word.as_str()) {
            if !placed {
                argv.push(url.to_owned());
                placed = true;
            }
        } else if !DROPPED_CODES.contains(&word.as_str()) {
            argv.push(word.replace("%%", "%"));
        }
    }
    if !placed {
        argv.push(url.to_owned());
    }
    Some(argv)
}

/// The words of an `Exec` line as the Desktop Entry spec splits it: blanks
/// separate them, and a `"…"` part may hold blanks and the escapes `\"`,
/// `` \` ``, `\$` and `\\`. `None` for an unclosed quote or another escape.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn exec_words(exec: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = exec.chars();
    while let Some(next) = chars.next() {
        if next == '"' {
            quoted_part(&mut chars, word.get_or_insert_with(String::new))?;
        } else if next.is_whitespace() {
            words.extend(word.take());
        } else {
            word.get_or_insert_with(String::new).push(next);
        }
    }
    words.extend(word);
    Some(words)
}

/// Reads a quoted part of an `Exec` word up to its closing quote into
/// `word`; `None` when the quote never closes or holds an escape the spec
/// does not allow.
#[cfg(any(all(unix, not(target_os = "macos")), test))]
fn quoted_part(chars: &mut std::str::Chars<'_>, word: &mut String) -> Option<()> {
    loop {
        match chars.next()? {
            '"' => return Some(()),
            '\\' => match chars.next()? {
                escaped @ ('"' | '`' | '$' | '\\') => word.push(escaped),
                _ => return None,
            },
            other => word.push(other),
        }
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "a test fails with the message of the precondition it lost"
)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const URL: &str = "file:///home/u/p.html#q1";

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn desktop_exec_replaces_the_url_code() {
        assert_eq!(
            desktop_exec_argv("firefox %u", URL),
            Some(argv(&["firefox", URL]))
        );
        assert_eq!(
            desktop_exec_argv("/usr/bin/google-chrome-stable %U", URL),
            Some(argv(&["/usr/bin/google-chrome-stable", URL]))
        );
        assert_eq!(
            desktop_exec_argv("chromium --incognito %U %F", URL),
            Some(argv(&["chromium", "--incognito", URL])),
            "only the first URL code takes the URL"
        );
    }

    #[test]
    fn desktop_exec_appends_the_url_without_a_code() {
        assert_eq!(
            desktop_exec_argv("epiphany", URL),
            Some(argv(&["epiphany", URL]))
        );
        assert_eq!(desktop_exec_argv("", URL), None);
        assert_eq!(desktop_exec_argv("   ", URL), None);
    }

    #[test]
    fn desktop_exec_honours_quotes_and_drops_other_codes() {
        assert_eq!(
            desktop_exec_argv(r#""/opt/My Browser/run" --new-window %i %u %c"#, URL),
            Some(argv(&["/opt/My Browser/run", "--new-window", URL]))
        );
        assert_eq!(
            desktop_exec_argv(r#"env "A=1 2" "b \"x\" \`y\` \$z \\w" %F"#, URL),
            Some(argv(&["env", "A=1 2", r#"b "x" `y` $z \w"#, URL]))
        );
        assert_eq!(
            desktop_exec_argv(r#""/a b/c" %%foo %u"#, URL),
            Some(argv(&["/a b/c", "%foo", URL]))
        );
        assert_eq!(
            desktop_exec_argv(r#""/a b/c %u"#, URL),
            None,
            "an unclosed quote"
        );
        assert_eq!(
            desktop_exec_argv(r#""\n" %u"#, URL),
            None,
            "an escape the spec does not allow"
        );
    }

    #[test]
    fn desktop_entry_exec_reads_only_the_main_group() {
        let contents = "[Desktop Action new-window]\nExec=firefox --new-window %u\n\n\
                        [Desktop Entry]\nName=Firefox\n# Exec=commented\nExec=firefox %u\n\n\
                        [Desktop Action new-private-window]\nExec=firefox --private-window %u\n";
        assert_eq!(desktop_entry_exec(contents), Some("firefox %u".to_owned()));
        assert_eq!(
            desktop_entry_exec("[Desktop Entry]\r\nExec = firefox %u\r\n"),
            Some("firefox %u".to_owned()),
            "spaces around `=` and CRLF line ends"
        );
        assert_eq!(
            desktop_entry_exec("[Desktop Action a]\nExec=firefox %u\n"),
            None
        );
    }

    #[test]
    fn xdg_application_dirs_follow_the_spec_defaults() {
        let dirs = |paths: &[&str]| paths.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(
            xdg_application_dirs(None, Some("/home/u"), None),
            dirs(&[
                "/home/u/.local/share/applications",
                "/usr/local/share/applications",
                "/usr/share/applications"
            ])
        );
        assert_eq!(
            xdg_application_dirs(Some(""), Some("/home/u"), Some("")),
            xdg_application_dirs(None, Some("/home/u"), None),
            "an empty variable counts as unset"
        );
        assert_eq!(
            xdg_application_dirs(Some("/d/home"), Some("/home/u"), Some("/x/share/:/y::rel")),
            dirs(&[
                "/d/home/applications",
                "/x/share/applications",
                "/y/applications"
            ]),
            "explicit values in order; empty and relative entries are skipped"
        );
        assert_eq!(
            xdg_application_dirs(None, None, Some("/y")),
            dirs(&["/y/applications"])
        );
    }

    #[test]
    fn flatpak_forwarding_falls_back_to_the_plain_file() {
        let exec = "/usr/bin/flatpak run --branch=stable --arch=x86_64 --command=firefox \
                    --file-forwarding org.mozilla.firefox @@u %u @@";
        let argv = desktop_exec_argv(exec, URL).expect("a flatpak Exec line parses");
        assert!(forwards_files(&argv), "{argv:?}");
        let argv = desktop_exec_argv("flatpak run org.x.Browser @@ %F @@", URL).expect("parses");
        assert!(forwards_files(&argv), "{argv:?}");
        let argv = desktop_exec_argv("firefox %u", URL).expect("parses");
        assert!(!forwards_files(&argv), "{argv:?}");
    }

    #[test]
    fn a_terminal_browser_falls_back_to_the_plain_file() {
        assert_eq!(
            browser_exec("[Desktop Entry]\nExec=lynx %u\nTerminal=true\n"),
            Err("the browser runs in a terminal".to_owned())
        );
        assert_eq!(
            browser_exec("[Desktop Entry]\nExec=firefox %u\nTerminal=false\n"),
            Ok("firefox %u".to_owned())
        );
        assert_eq!(
            browser_exec("[Desktop Entry]\nExec=firefox %u\n[Desktop Action a]\nTerminal=true\n"),
            Ok("firefox %u".to_owned()),
            "only the main group's Terminal counts"
        );
        assert_eq!(
            browser_exec("[Desktop Entry]\nName=x\n"),
            Err("no Exec line".to_owned())
        );
    }

    #[test]
    fn mac_script_takes_the_url_only_from_argv() {
        assert!(SCRIPT.contains("function run(argv)"), "{SCRIPT}");
        assert!(SCRIPT.contains("openLocation(argv[0])"), "{SCRIPT}");
        assert!(
            !SCRIPT.contains("{}") && !SCRIPT.contains("{url"),
            "the URL is never formatted into the script: {SCRIPT}"
        );
    }
}
