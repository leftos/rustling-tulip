//! VS Code `.code-workspace` file detection and parsing.

use anyhow::{Context as _, anyhow};
use protocol::{VscodeWorkspaceFolder, VscodeWorkspaceSuggestion};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
struct VscodeWorkspaceFile {
    folders: Vec<VscodeFolderEntry>,
}

#[derive(Debug, Deserialize)]
struct VscodeFolderEntry {
    path: String,
    name: Option<String>,
}

/// Find `.code-workspace` files inside `repo_dir`. We don't recurse — these
/// almost always sit at the top of a repo or in a sibling directory.
pub fn find_workspace_files(repo_dir: &Path) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return hits;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("code-workspace"))
        {
            hits.push(path);
        }
    }
    hits
}

/// Parse a `.code-workspace` file, resolving every folder path against the
/// file's directory. Tolerates `// line` and `/* block */` comments.
pub fn parse_workspace_file(
    path: &Path,
    known_repos: &[(String, String)],
) -> anyhow::Result<VscodeWorkspaceSuggestion> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let stripped = strip_jsonc_comments(&text);
    let parsed: VscodeWorkspaceFile =
        serde_json::from_str(&stripped).with_context(|| format!("parsing {}", path.display()))?;

    let base_dir = path
        .parent()
        .ok_or_else(|| anyhow!("workspace file has no parent dir"))?;
    let stem = path.file_stem().map_or_else(
        || "workspace".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );

    let mut folders = Vec::with_capacity(parsed.folders.len());
    for entry in parsed.folders {
        let raw = PathBuf::from(&entry.path);
        let abs = if raw.is_absolute() {
            raw
        } else {
            base_dir.join(raw)
        };
        let canonical = std::fs::canonicalize(&abs)
            .map(|p| crate::paths::simplify_path(&p))
            .unwrap_or(abs);
        let canonical_str = canonical.to_string_lossy().into_owned();
        let matched_repo_id = known_repos
            .iter()
            .find(|(_, p)| paths_eq(p, &canonical_str))
            .map(|(id, _)| id.clone());
        folders.push(VscodeWorkspaceFolder {
            path: canonical_str,
            name: entry.name,
            matched_repo_id,
        });
    }

    Ok(VscodeWorkspaceSuggestion {
        source_path: path.to_string_lossy().into_owned(),
        suggested_name: stem,
        folders,
    })
}

fn paths_eq(a: &str, b: &str) -> bool {
    a.replace('\\', "/").to_lowercase() == b.replace('\\', "/").to_lowercase()
}

/// Strip `// line` and `/* block */` comments from JSON-with-comments. Skips
/// occurrences inside string literals.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_str = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            out.push(c as char);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push('"');
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() {
            let next = bytes[i + 1];
            if next == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}
