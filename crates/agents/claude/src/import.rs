//! Reading a Claude session back from the tracer log of a run the app did
//! not record.

use crate::options::ClaudeProvider;
use crate::{ClaudeOptions, provider_for_model};
use agent_api::{ImportedRun, TracerRun};
use std::path::Path;

/// The import revision: revision 3 stores the run's options and trust on the
/// history entry, so every older import is read again from its tracer log.
pub(crate) const IMPORT_REV: u32 = 3;

/// The Claude run `run` describes: its program's file stem starts with
/// `claude`. Its `--add-dir`s are its extra folders, its last `--model` its
/// model and the provider whose table names that model, and
/// `--dangerously-skip-permissions` makes it trusted.
pub(crate) fn import_tracer_run(run: &TracerRun<'_>) -> Option<ImportedRun<ClaudeOptions>> {
    if !program_stem(run.program)?.starts_with("claude") {
        return None;
    }
    let model = model_arg(run.args);
    Some(ImportedRun {
        program_name: "claude".to_owned(),
        extra_dirs: add_dir_args(run.args),
        options: ClaudeOptions {
            permission_mode: None,
            provider: imported_provider(model.as_deref()),
            model,
        },
        trusted: run
            .args
            .iter()
            .any(|arg| arg == "--dangerously-skip-permissions"),
    })
}

/// The provider an imported session that ran `model` was on.
fn imported_provider(model: Option<&str>) -> ClaudeProvider {
    model.map_or(ClaudeProvider::Anthropic, provider_for_model)
}

/// The lowercase file stem of `program`, whichever separator its path uses.
fn program_stem(program: &str) -> Option<String> {
    let leaf = program.rsplit(['/', '\\']).next()?;
    let stem = Path::new(leaf).file_stem()?.to_str()?.to_lowercase();
    (!stem.is_empty()).then_some(stem)
}

/// Every `--add-dir <path>` value in `args`, in order.
fn add_dir_args(args: &[String]) -> Vec<String> {
    args.windows(2)
        .filter(|pair| pair[0] == "--add-dir")
        .map(|pair| pair[1].clone())
        .collect()
}

/// The value of the last `--model <m>` or `--model=<m>` in `args`.
fn model_arg(args: &[String]) -> Option<String> {
    let mut model = None;
    for (i, arg) in args.iter().enumerate() {
        if let Some(value) = arg.strip_prefix("--model=") {
            model = Some(value.to_owned());
        } else if arg == "--model"
            && let Some(value) = args.get(i + 1)
        {
            model = Some(value.clone());
        }
    }
    model.filter(|m| !m.is_empty())
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests assert preconditions with expect; failure messages aid debugging"
)]
mod tests {
    use crate::options::ClaudeProvider;
    use crate::{ClaudeOptions, ClaudePlugin};
    use agent_api::{Agent, ImportedRun, TracerRun};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_string()).collect()
    }

    fn import(program: &str, list: &[&str]) -> Option<ImportedRun<ClaudeOptions>> {
        let args = args(list);
        ClaudePlugin.import_tracer_run(&TracerRun {
            program,
            args: &args,
            cwd: r"D:\yaat",
        })
    }

    #[test]
    fn import_reads_model_skip_and_add_dirs() {
        let run = import(
            r"C:\Users\me\.local\bin\claude.exe",
            &[
                "--add-dir",
                r"D:\yaat-server",
                "--model",
                "opus",
                "--dangerously-skip-permissions",
                "--add-dir",
                r"D:\yaat-web",
            ],
        )
        .expect("a claude program imports");
        assert_eq!(
            run,
            ImportedRun {
                program_name: "claude".to_string(),
                extra_dirs: args(&[r"D:\yaat-server", r"D:\yaat-web"]),
                options: ClaudeOptions {
                    permission_mode: None,
                    provider: ClaudeProvider::Anthropic,
                    model: Some("opus".to_string()),
                },
                trusted: true,
            }
        );
        let bare = import("claude", &[]).expect("a bare claude imports");
        assert_eq!(bare.extra_dirs, Vec::<String>::new());
        assert_eq!(bare.options, ClaudeOptions::default());
        assert!(!bare.trusted);
    }

    #[test]
    fn import_ignores_a_dangling_add_dir() {
        let run = import(
            "claude",
            &["--add-dir", "a", "-x", "--add-dir", "b", "--add-dir"],
        )
        .expect("claude imports");
        assert_eq!(run.extra_dirs, args(&["a", "b"]));
    }

    #[test]
    fn import_reads_model_equals_form() {
        let run = import(
            "/usr/bin/Claude-Code",
            &["--model", "opus", "--model=sonnet"],
        )
        .expect("a claude-prefixed stem imports");
        assert_eq!(run.options.model.as_deref(), Some("sonnet"));
        let empty = import("claude", &["--model="]).expect("claude imports");
        assert_eq!(empty.options.model, None, "an empty model is none");
    }

    #[test]
    fn import_deepseek_model_selects_deepseek_provider() {
        let run = import("claude.cmd", &["--model", "DeepSeek-Flash"]).expect("claude imports");
        assert_eq!(run.options.provider, ClaudeProvider::Deepseek);
        assert_eq!(run.options.model.as_deref(), Some("DeepSeek-Flash"));
    }

    #[test]
    fn import_ignores_a_shell_program() {
        for program in [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            "cmd.exe",
            "bash",
            "",
        ] {
            assert_eq!(import(program, &["--model", "opus"]), None, "{program}");
        }
    }

    #[test]
    fn import_rev_is_3() {
        assert_eq!(ClaudePlugin.import_rev(), 3);
    }

    #[test]
    fn claude_liveness_tokens_are_claude_and_node() {
        assert_eq!(ClaudePlugin.liveness_tokens(), ["claude", "node"]);
    }
}
