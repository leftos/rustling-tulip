//! The `/` catalog: the commands and skills a Claude session can invoke, as
//! read from Claude Code's home and the session's own folders.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use agent_api::{CatalogContext, CatalogEntry, CatalogKind, CatalogSource};
use tracing::debug;

use crate::transcripts::{CONFIG_DIR_VAR, home_from_config_dir};

/// The most entries a catalog answers.
const MAX_ENTRIES: usize = 500;
/// The largest catalog file read, in bytes; a larger one is listed with no
/// description.
const MAX_FILE_BYTES: u64 = 64 * 1024;
/// How many folders below a commands folder the walk descends.
const MAX_DEPTH: usize = 4;
/// The longest description kept, in characters.
const DESCRIPTION_MAX_CHARS: usize = 120;

/// The built-in commands, each with its one-line description and whether it
/// opens an overlay or dialog the chat cannot answer.
const BUILT_INS: [(&str, &str, bool); 23] = [
    (
        "clear",
        "Clear the conversation and free its context.",
        false,
    ),
    (
        "compact",
        "Compress the conversation into a summary.",
        false,
    ),
    ("context", "Show what fills the context window.", false),
    ("cost", "Show the session's token usage and cost.", true),
    ("help", "List the available commands.", true),
    (
        "init",
        "Write a CLAUDE.md with guidance for this repository.",
        false,
    ),
    ("memory", "Edit the agent's memory files.", true),
    (
        "model",
        "Choose the model and effort the session runs with.",
        true,
    ),
    ("config", "Open the agent's settings.", true),
    ("resume", "Resume an earlier conversation.", true),
    (
        "code-review",
        "Review the current changes or a pull request.",
        false,
    ),
    (
        "status",
        "Show the session's account, model and connection status.",
        true,
    ),
    ("mcp", "Manage the MCP servers a session can use.", true),
    (
        "permissions",
        "Manage what the agent may do without asking.",
        true,
    ),
    (
        "hooks",
        "Manage the hooks that run around the agent's actions.",
        true,
    ),
    ("doctor", "Check the installation for problems.", false),
    ("login", "Sign in to a Claude account.", true),
    ("logout", "Sign out of the current Claude account.", true),
    ("export", "Export the conversation.", true),
    ("add-dir", "Add another folder to the session.", true),
    (
        "rewind",
        "Go back to an earlier point in the conversation.",
        true,
    ),
    ("usage", "Show the account's usage and limits.", true),
    ("plugin", "Manage plugins and their marketplaces.", true),
];

/// The commands and skills `ctx`'s session can invoke, sorted by name.
pub(crate) fn catalog(ctx: &CatalogContext<'_>) -> Vec<CatalogEntry> {
    catalog_from(
        std::env::var_os(CONFIG_DIR_VAR),
        ctx.home.map(Path::to_path_buf),
        ctx.roots,
    )
}

/// [`catalog`] from the `CLAUDE_CONFIG_DIR` value `env` and the home folder
/// `home`, over `roots`.
fn catalog_from(
    env: Option<OsString>,
    home: Option<PathBuf>,
    roots: &[PathBuf],
) -> Vec<CatalogEntry> {
    catalog_with(env, home, roots, Budget::ONE_ROOT)
}

/// [`catalog_from`], reading each `.claude` folder with `budget`.
fn catalog_with(
    env: Option<OsString>,
    home: Option<PathBuf>,
    roots: &[PathBuf],
    budget: Budget,
) -> Vec<CatalogEntry> {
    let mut entries = built_ins();
    if let Some(config) = home_from_config_dir(env, home) {
        read(&config, &CatalogSource::User, budget, &mut entries);
    }
    for root in roots {
        read(
            &root.join(".claude"),
            &CatalogSource::Project,
            budget,
            &mut entries,
        );
    }
    ordered(entries)
}

/// How much of one `.claude` folder's tree a read lists.
#[derive(Clone, Copy)]
struct Budget {
    /// The folders the read may still enter.
    folders: usize,
    /// The files the read may still look at.
    files: usize,
}

impl Budget {
    /// The budget one `.claude` folder is read with.
    const ONE_ROOT: Self = Self {
        folders: 2000,
        files: 2000,
    };
}

/// Append the commands and skills under `claude` — one session's `.claude`
/// folder — to `out`, with a fresh `budget`.
fn read(claude: &Path, source: &CatalogSource, budget: Budget, out: &mut Vec<CatalogEntry>) {
    let mut walk = Walk {
        source,
        out,
        budget,
        path: Vec::new(),
    };
    walk.commands(&claude.join("commands"));
    walk.skills(&claude.join("skills"));
}

/// One `.claude` folder's read: where its entries come from, what it has left
/// of the budget, and the canonical folders on the path it is walking.
struct Walk<'a> {
    /// Where the entries it lists come from.
    source: &'a CatalogSource,
    /// The entries listed so far.
    out: &'a mut Vec<CatalogEntry>,
    /// What is left of the folder's budget.
    budget: Budget,
    /// The canonical folders on the current path, so a link cycle is never
    /// entered.
    path: Vec<PathBuf>,
}

/// [`BUILT_INS`] as catalog entries.
fn built_ins() -> Vec<CatalogEntry> {
    BUILT_INS
        .iter()
        .map(|&(name, description, opens_terminal)| CatalogEntry {
            name: name.to_owned(),
            kind: CatalogKind::Command,
            description: Some(description.to_owned()),
            source: CatalogSource::BuiltIn,
            opens_terminal,
        })
        .collect()
}

/// `entries` sorted by name, one entry kept per name and at most
/// [`MAX_ENTRIES`] of them.
fn ordered(mut entries: Vec<CatalogEntry>) -> Vec<CatalogEntry> {
    entries.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| precedence(left).cmp(&precedence(right)))
    });
    let mut kept: Vec<CatalogEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if kept.last().map(|last| last.name.as_str()) == Some(entry.name.as_str()) {
            continue;
        }
        kept.push(entry);
    }
    kept.truncate(MAX_ENTRIES);
    kept
}

/// How strongly `entry` wins a name clash: project over user over built-in,
/// and a command over a skill from the same source.
fn precedence(entry: &CatalogEntry) -> (u8, u8) {
    let source = match entry.source {
        CatalogSource::Project => 0,
        CatalogSource::User => 1,
        CatalogSource::BuiltIn => 2,
    };
    let kind = match entry.kind {
        CatalogKind::Command => 0,
        CatalogKind::Skill => 1,
    };
    (source, kind)
}

impl Walk<'_> {
    /// Append the `.md` commands under `dir`.
    fn commands(&mut self, dir: &Path) {
        if !self.enter(dir) {
            return;
        }
        self.commands_at(dir, &[], 0);
        self.path.pop();
    }

    /// Append the `.md` commands under `dir`, which is on the path and whose
    /// folders so far are `folders`. `depth` is how many folders below the
    /// commands folder it stands; the walk descends at most [`MAX_DEPTH`]
    /// folders, and a command is named for its file's stem, with `folders`
    /// before it joined by `:`.
    fn commands_at(&mut self, dir: &Path, folders: &[String], depth: usize) {
        for entry in read_dir_entries(dir) {
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                debug!(folder = %dir.display(), "skipping a catalog entry whose name is not UTF-8");
                continue;
            };
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(err) => {
                    debug!(path = %path.display(), error = %err, "skipping an unreadable catalog entry");
                    continue;
                }
            };
            if metadata.is_dir() {
                if depth >= MAX_DEPTH {
                    debug!(path = %path.display(), "skipping a catalog folder past the depth bound");
                    continue;
                }
                if !self.enter(&path) {
                    continue;
                }
                let mut deeper = folders.to_vec();
                deeper.push(name);
                self.commands_at(&path, &deeper, depth + 1);
                self.path.pop();
            } else if metadata.is_file() {
                self.command(&path, &name, folders);
            } else {
                debug!(path = %path.display(), "skipping a catalog entry that is not a regular file");
            }
        }
    }

    /// Append the command whose file is `path` and whose name is `name`, in
    /// `folders`.
    fn command(&mut self, path: &Path, name: &str, folders: &[String]) {
        let Some(stem) = name.strip_suffix(".md") else {
            return;
        };
        if stem.is_empty() || !self.take_file() {
            return;
        }
        let Some(body) = read_catalog_file(path) else {
            return;
        };
        let mut parts = folders.to_vec();
        parts.push(stem.to_owned());
        self.out.push(CatalogEntry {
            name: parts.join(":"),
            kind: CatalogKind::Command,
            description: body.description(command_description),
            source: self.source.clone(),
            opens_terminal: false,
        });
    }

    /// Append the skills under `dir`: each folder's `SKILL.md`, named by its
    /// frontmatter `name` or else the folder's own name.
    fn skills(&mut self, dir: &Path) {
        if !self.enter(dir) {
            return;
        }
        for entry in read_dir_entries(dir) {
            let path = entry.path();
            if !fs::metadata(&path).is_ok_and(|metadata| metadata.is_dir()) {
                continue;
            }
            let Some(folder) = entry.file_name().to_str().map(str::to_owned) else {
                debug!(folder = %dir.display(), "skipping a skill whose name is not UTF-8");
                continue;
            };
            self.skill(&path, folder);
        }
        self.path.pop();
    }

    /// Append the skill in folder `dir`, whose folder name is `folder`.
    fn skill(&mut self, dir: &Path, folder: String) {
        if !self.take_file() {
            return;
        }
        let Some(body) = read_catalog_file(&dir.join("SKILL.md")) else {
            return;
        };
        let (name, description) = match &body {
            Body::Text(text) => {
                let front = split_frontmatter(text).0.unwrap_or("");
                let name = front_value(front, "name").unwrap_or(folder);
                (
                    name,
                    front_value(front, "description").map(|line| cut(&line)),
                )
            }
            Body::Oversized => (folder, None),
        };
        self.out.push(CatalogEntry {
            name,
            kind: CatalogKind::Skill,
            description,
            source: self.source.clone(),
            opens_terminal: false,
        });
    }

    /// Whether `dir` may be entered: its canonical path is not already on the
    /// path the walk is on, and the folder budget has some left. A folder it
    /// enters is pushed on the path, for the caller to pop.
    fn enter(&mut self, dir: &Path) -> bool {
        if self.budget.folders == 0 {
            debug!(path = %dir.display(), "stopping a root at its folder visit budget");
            return false;
        }
        let canonical = match fs::canonicalize(dir) {
            Ok(canonical) => canonical,
            Err(err) => {
                debug!(folder = %dir.display(), error = %err, "skipping an unreadable catalog folder");
                return false;
            }
        };
        if self.path.contains(&canonical) {
            debug!(path = %dir.display(), "skipping a catalog folder already on this path");
            return false;
        }
        self.path.push(canonical);
        self.budget.folders -= 1;
        true
    }

    /// Whether the file budget has some left, spending one.
    fn take_file(&mut self) -> bool {
        if self.budget.files == 0 {
            debug!("stopping a root at its file visit budget");
            return false;
        }
        self.budget.files -= 1;
        true
    }
}

/// What reading a catalog file answered.
enum Body {
    /// The file's text, at most [`MAX_FILE_BYTES`].
    Text(String),
    /// The file is larger than [`MAX_FILE_BYTES`]; it is listed with no
    /// description.
    Oversized,
}

impl Body {
    /// The description `describe` reads from the file's text; `None` for a
    /// file too large to read.
    fn description(&self, describe: fn(&str) -> Option<String>) -> Option<String> {
        match self {
            Self::Text(text) => describe(text),
            Self::Oversized => None,
        }
    }
}

/// `path`'s text, or that it is too large; `None` for a file that cannot be
/// read. Only a regular file is read, so a device, a pipe or a folder that
/// carries the name is skipped.
fn read_catalog_file(path: &Path) -> Option<Body> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) => {
            debug!(path = %path.display(), error = %err, "skipping an unreadable catalog file");
            return None;
        }
    };
    if !metadata.is_file() {
        debug!(path = %path.display(), "skipping a catalog file that is not a regular file");
        return None;
    }
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            debug!(path = %path.display(), error = %err, "skipping an unreadable catalog file");
            return None;
        }
    };
    match read_bounded(file) {
        Ok(body) => Some(body),
        Err(err) => {
            debug!(path = %path.display(), error = %err, "skipping an unreadable catalog file");
            None
        }
    }
}

/// The text `read` answers, up to [`MAX_FILE_BYTES`]; [`Body::Oversized`]
/// when it holds more. The read is capped one byte past the limit, so a file
/// whose reported size lies, a pipe or a device never reads forever.
fn read_bounded(read: impl Read) -> std::io::Result<Body> {
    let mut text = String::new();
    read.take(MAX_FILE_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_FILE_BYTES {
        Ok(Body::Oversized)
    } else {
        Ok(Body::Text(text))
    }
}

/// `dir`'s entries; an unreadable folder or entry is logged and skipped.
fn read_dir_entries(dir: &Path) -> Vec<fs::DirEntry> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) => {
            debug!(folder = %dir.display(), error = %err, "skipping an unreadable catalog folder");
            return Vec::new();
        }
    };
    let mut entries = Vec::new();
    for entry in read {
        match entry {
            Ok(entry) => entries.push(entry),
            Err(err) => {
                debug!(folder = %dir.display(), error = %err, "skipping an unreadable catalog entry");
            }
        }
    }
    entries
}

/// The description of the command whose file text is `text`: its frontmatter
/// `description`, else the first non-empty body line that is not a heading,
/// cut to [`DESCRIPTION_MAX_CHARS`].
fn command_description(text: &str) -> Option<String> {
    let (front, body) = split_frontmatter(text);
    let front = front.and_then(|front| front_value(front, "description"));
    front
        .or_else(|| first_body_line(body))
        .map(|line| cut(&line))
}

/// The first non-empty line of `body` that is not a heading marker.
fn first_body_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

/// At most [`DESCRIPTION_MAX_CHARS`] characters of `text`.
fn cut(text: &str) -> String {
    text.chars().take(DESCRIPTION_MAX_CHARS).collect()
}

/// `text` split into its frontmatter block and its body: a first line of
/// `---` closed by a later line that is exactly `---` holds the frontmatter.
/// Anything else is all body. One byte order mark before the block is
/// dropped, as PowerShell's `Set-Content -Encoding UTF8` writes one.
fn split_frontmatter(text: &str) -> (Option<&str>, &str) {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let Some(rest) = text.strip_prefix("---") else {
        return (None, text);
    };
    let Some(rest) = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
    else {
        return (None, text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return (Some(&rest[..offset]), &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    (None, text)
}

/// The value of frontmatter key `key` in `front`, quoted or bare, when it is
/// neither missing nor empty.
fn front_value(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        let Some((left, right)) = line.split_once(':') else {
            continue;
        };
        if left.trim() != key {
            continue;
        }
        let value = unquote(right.trim());
        return (!value.is_empty()).then(|| value.to_owned());
    }
    None
}

/// `value` without one matching pair of `"` or `'` around it.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 && matches!(bytes[0], b'"' | b'\'') && bytes[bytes.len() - 1] == bytes[0] {
        return &value[1..value.len() - 1];
    }
    value
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert scratch setup preconditions with expect for clear failure messages"
)]
mod tests {
    use super::{
        BUILT_INS, Body, Budget, MAX_FILE_BYTES, catalog_from, catalog_with, command_description,
        read_bounded,
    };
    use agent_api::{CatalogEntry, CatalogKind, CatalogSource};
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    /// RAII scratch folder under the OS temp root. Drop removes the tree.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("rt-catalog-{label}-{}", Uuid::new_v4().simple()));
            fs::create_dir_all(&path).expect("create scratch dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn home(&self) -> PathBuf {
            self.path.join("home")
        }

        /// Writes `relative` under the scratch root.
        fn write(&self, relative: &str, body: &str) {
            self.write_bytes(relative, body.as_bytes());
        }

        /// Writes `relative` under the scratch root with `body` as its bytes.
        fn write_bytes(&self, relative: &str, body: &[u8]) {
            let path = self.path.join(relative);
            fs::create_dir_all(path.parent().expect("a file has a parent"))
                .expect("create file's folder");
            fs::write(path, body).expect("write file");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn find<'a>(entries: &'a [CatalogEntry], name: &str) -> &'a CatalogEntry {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("the catalog has no `{name}` entry"))
    }

    /// An in-memory file that counts the bytes read from it.
    struct Spy<'a> {
        bytes: &'a [u8],
        read: usize,
    }

    impl std::io::Read for Spy<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = &self.bytes[self.read.min(self.bytes.len())..];
            let taken = left.len().min(buf.len());
            buf[..taken].copy_from_slice(&left[..taken]);
            self.read += taken;
            Ok(taken)
        }
    }

    /// Makes `link` a folder link to `target`: a junction on Windows, which
    /// needs no elevation, and a symlink elsewhere.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            // mklink is cmd's own command, and its arguments take no forward
            // slashes.
            let link = link.to_string_lossy().replace('/', "\\");
            let target = target.to_string_lossy().replace('/', "\\");
            let status = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .status()
                .expect("run mklink");
            assert!(status.success(), "mklink /J {link} -> {target} failed");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("make the folder link");
    }

    #[test]
    fn catalog_lists_user_and_project_commands_with_descriptions() {
        let scratch = Scratch::new("both");
        scratch.write(
            "home/.claude/commands/deploy.md",
            "---\ndescription: \"Ship the release\"\n---\n\nbody\n",
        );
        scratch.write(
            "repo/.claude/commands/lint.md",
            "# Lint\n\nRun the linter over changed files.\n",
        );
        scratch.write(
            "home/.claude/commands/heading-only.md",
            "# Only a heading\n",
        );
        scratch.write(
            "home/.claude/commands/unclosed.md",
            "---\ndescription: never closed\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[scratch.path().join("repo")]);

        let deploy = find(&entries, "deploy");
        assert_eq!(deploy.source, CatalogSource::User);
        assert_eq!(deploy.kind, CatalogKind::Command);
        assert_eq!(deploy.description.as_deref(), Some("Ship the release"));
        assert!(!deploy.opens_terminal);

        let lint = find(&entries, "lint");
        assert_eq!(lint.source, CatalogSource::Project);
        assert_eq!(
            lint.description.as_deref(),
            Some("Run the linter over changed files.")
        );

        assert_eq!(find(&entries, "heading-only").description, None);
        assert_eq!(
            find(&entries, "unclosed").description.as_deref(),
            Some("---")
        );
    }

    #[test]
    fn catalog_names_a_nested_command_with_its_folder() {
        let scratch = Scratch::new("nested");
        scratch.write("home/.claude/commands/git/release.md", "Cut a release.\n");
        scratch.write(
            "home/.claude/commands/git/tag/annotate.md",
            "Annotate a tag.\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[]);

        assert_eq!(
            find(&entries, "git:release").description.as_deref(),
            Some("Cut a release.")
        );
        assert_eq!(
            find(&entries, "git:tag:annotate").description.as_deref(),
            Some("Annotate a tag.")
        );
        assert!(!entries.iter().any(|entry| entry.name == "release"));
    }

    #[test]
    fn catalog_reads_a_skill_name_and_description_from_frontmatter() {
        let scratch = Scratch::new("skills");
        scratch.write(
            "home/.claude/skills/harness-retro/SKILL.md",
            "---\nname: retro\ndescription: Review the harness.\n---\n\n# Retro\n",
        );
        scratch.write(
            "home/.claude/skills/plain/SKILL.md",
            "---\ndescription: No name given.\n---\n",
        );
        scratch.write(
            "repo/.claude/skills/deploy/SKILL.md",
            "---\nname: ship\ndescription: Ship the release.\n---\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[scratch.path().join("repo")]);

        let retro = find(&entries, "retro");
        assert_eq!(retro.kind, CatalogKind::Skill);
        assert_eq!(retro.source, CatalogSource::User);
        assert_eq!(retro.description.as_deref(), Some("Review the harness."));
        assert!(!retro.opens_terminal);

        let plain = find(&entries, "plain");
        assert_eq!(plain.description.as_deref(), Some("No name given."));

        let ship = find(&entries, "ship");
        assert_eq!(ship.source, CatalogSource::Project);
        assert_eq!(ship.description.as_deref(), Some("Ship the release."));
        assert!(!entries.iter().any(|entry| entry.name == "deploy"));
    }

    #[test]
    fn catalog_prefers_project_over_user_over_built_in() {
        let scratch = Scratch::new("clash");
        scratch.write("home/.claude/commands/review.md", "User review.\n");
        scratch.write("repo/.claude/commands/review.md", "Project review.\n");
        scratch.write("home/.claude/commands/help.md", "User help.\n");
        scratch.write("repo/.claude/commands/cost.md", "Project cost.\n");
        scratch.write("repo/.claude/commands/deploy.md", "Command deploy.\n");
        scratch.write(
            "repo/.claude/skills/deploy/SKILL.md",
            "---\ndescription: Skill deploy.\n---\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[scratch.path().join("repo")]);

        let review = find(&entries, "review");
        assert_eq!(review.source, CatalogSource::Project);
        assert_eq!(review.description.as_deref(), Some("Project review."));
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name == "review")
                .count(),
            1
        );

        let help = find(&entries, "help");
        assert_eq!(help.source, CatalogSource::User);
        assert_eq!(help.description.as_deref(), Some("User help."));

        let cost = find(&entries, "cost");
        assert_eq!(cost.source, CatalogSource::Project);
        assert_eq!(cost.description.as_deref(), Some("Project cost."));

        let deploy = find(&entries, "deploy");
        assert_eq!(deploy.kind, CatalogKind::Command);
        assert_eq!(deploy.description.as_deref(), Some("Command deploy."));

        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the catalog is not sorted by name");
    }

    #[test]
    fn catalog_flags_overlay_built_ins_as_opening_the_terminal() {
        let entries = catalog_from(None, None, &[]);
        assert_eq!(entries.len(), BUILT_INS.len());

        let in_terminal = [
            "model",
            "config",
            "resume",
            "mcp",
            "permissions",
            "hooks",
            "memory",
            "login",
            "logout",
            "status",
            "usage",
            "rewind",
            "export",
            "cost",
            "help",
            "add-dir",
            "plugin",
        ];
        let in_chat = [
            "clear",
            "compact",
            "context",
            "init",
            "code-review",
            "doctor",
        ];
        for name in in_terminal {
            let entry = find(&entries, name);
            assert!(entry.opens_terminal, "`{name}` does not open the terminal");
            assert_eq!(entry.source, CatalogSource::BuiltIn);
            assert_eq!(entry.kind, CatalogKind::Command);
            assert!(
                entry
                    .description
                    .as_deref()
                    .is_some_and(|line| !line.is_empty()),
                "`{name}` has no description"
            );
        }
        for name in in_chat {
            assert!(
                !find(&entries, name).opens_terminal,
                "`{name}` opens the terminal"
            );
        }
    }

    #[test]
    fn built_ins_flag_the_overlays_the_spike_measured() {
        let mut flagged: Vec<&str> = BUILT_INS
            .iter()
            .filter(|(_, _, opens_terminal)| *opens_terminal)
            .map(|(name, _, _)| *name)
            .collect();
        flagged.sort_unstable();
        let mut expected = vec![
            "model",
            "config",
            "resume",
            "memory",
            "status",
            "mcp",
            "permissions",
            "hooks",
            "login",
            "logout",
            "export",
            "add-dir",
            "rewind",
            "usage",
            "cost",
            "help",
            "plugin",
        ];
        expected.sort_unstable();
        assert_eq!(flagged, expected);
    }

    #[test]
    fn catalog_skips_unreadable_and_oversized_files() {
        let scratch = Scratch::new("bounds");
        scratch.write("home/.claude/commands/big.md", &"x".repeat(70_000));
        scratch.write_bytes("home/.claude/commands/binary.md", b"\xff\xfe not UTF-8");
        fs::create_dir_all(scratch.path().join("home/.claude/commands/notes.md"))
            .expect("create a folder named like a command");
        let entries = catalog_from(
            None,
            Some(scratch.home()),
            &[scratch.path().join("also-absent")],
        );

        assert_eq!(find(&entries, "big").description, None);
        assert!(!entries.iter().any(|entry| entry.name == "binary"));
        assert!(!entries.iter().any(|entry| entry.name == "notes"));
        assert_eq!(entries.len(), BUILT_INS.len() + 1);

        let absent = catalog_from(
            Some(OsString::from(scratch.path().join("absent"))),
            None,
            &[scratch.path().join("also-absent")],
        );
        assert_eq!(absent.len(), BUILT_INS.len());
    }

    #[test]
    fn catalog_honours_the_config_dir_override() {
        let scratch = Scratch::new("override");
        scratch.write("override/commands/from-override.md", "Override.\n");
        scratch.write("home/.claude/commands/from-home.md", "Home.\n");
        let roots: &[PathBuf] = &[];

        let overridden = catalog_from(
            Some(OsString::from(scratch.path().join("override"))),
            Some(scratch.home()),
            roots,
        );
        assert_eq!(
            find(&overridden, "from-override").description.as_deref(),
            Some("Override.")
        );
        assert!(!overridden.iter().any(|entry| entry.name == "from-home"));

        let empty = catalog_from(Some(OsString::new()), Some(scratch.home()), roots);
        assert_eq!(
            find(&empty, "from-home").description.as_deref(),
            Some("Home.")
        );
        assert!(!empty.iter().any(|entry| entry.name == "from-override"));
    }

    #[test]
    fn catalog_stops_at_four_folders_deep() {
        let scratch = Scratch::new("depth");
        scratch.write(
            "home/.claude/commands/a/b/c/d/deep.md",
            "Four folders down.\n",
        );
        scratch.write(
            "home/.claude/commands/a/b/c/d/e/deeper.md",
            "Five folders down.\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[]);

        assert!(entries.iter().any(|entry| entry.name == "a:b:c:d:deep"));
        assert!(!entries.iter().any(|entry| entry.name == "a:b:c:d:e:deeper"));
    }

    #[test]
    fn catalog_does_not_follow_a_link_cycle() {
        let scratch = Scratch::new("cycle");
        scratch.write("home/.claude/commands/loop.md", "Loop.\n");
        let commands = scratch.path().join("home/.claude/commands");
        link_dir(&commands, &commands.join("self"));
        let entries = catalog_from(None, Some(scratch.home()), &[]);

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.name.ends_with("loop"))
                .count(),
            1,
            "the walk lists a command more than once through a folder link"
        );
        assert!(
            !entries.iter().any(|entry| entry.name.contains("self:")),
            "the walk enters a folder link"
        );
    }

    #[test]
    fn catalog_stops_a_root_at_its_visit_budget() {
        let scratch = Scratch::new("budget");
        for index in 0..5 {
            scratch.write(&format!("home/.claude/commands/one-{index}.md"), "Body.\n");
            scratch.write(&format!("repo/.claude/commands/two-{index}.md"), "Body.\n");
        }
        let entries = catalog_with(
            None,
            Some(scratch.home()),
            &[scratch.path().join("repo")],
            Budget {
                folders: 8,
                files: 2,
            },
        );

        assert_eq!(
            entries.len(),
            BUILT_INS.len() + 4,
            "each `.claude` folder is read with its own file budget"
        );
    }

    #[test]
    fn catalog_reads_at_most_the_file_limit() {
        let limit = usize::try_from(MAX_FILE_BYTES).expect("the file limit fits a usize");
        let at_limit = vec![b'x'; limit];
        let body = read_bounded(&at_limit[..]).expect("a slice reads");
        assert!(
            matches!(body, Body::Text(_)),
            "a file at the limit is not read"
        );

        let over = vec![b'x'; limit + 10];
        let mut spy = Spy {
            bytes: &over,
            read: 0,
        };
        let body = read_bounded(&mut spy).expect("a slice reads");
        assert_eq!(
            body.description(command_description),
            None,
            "a file past the limit has a description"
        );
        assert!(
            spy.read as u64 <= MAX_FILE_BYTES + 1,
            "the reader read {} bytes of a file past the limit",
            spy.read
        );
    }

    #[test]
    fn catalog_skips_a_command_that_is_not_a_regular_file() {
        let scratch = Scratch::new("not-a-file");
        let folder = scratch.path().join("home/.claude/commands/x.md");
        fs::create_dir_all(&folder).expect("create a folder named like a command");
        let entries = catalog_from(None, Some(scratch.home()), &[]);

        assert!(!entries.iter().any(|entry| entry.name == "x"));
        assert_eq!(entries.len(), BUILT_INS.len());
    }

    #[test]
    fn catalog_reads_frontmatter_after_a_byte_order_mark() {
        let scratch = Scratch::new("bom");
        scratch.write(
            "home/.claude/commands/bom.md",
            "\u{feff}---\ndescription: Written by PowerShell\n---\n\nBody.\n",
        );
        let entries = catalog_from(None, Some(scratch.home()), &[]);

        assert_eq!(
            find(&entries, "bom").description.as_deref(),
            Some("Written by PowerShell")
        );
    }
}
