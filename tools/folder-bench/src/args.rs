//! A minimal argument parser: positionals and `--name value` flags.

use anyhow::{Context as _, ensure};

pub struct Parsed {
    positional: Vec<String>,
    flags: Vec<(String, String)>,
}

/// Split `args` into positionals and `--name value` flags, refusing a flag
/// not in `known`, one with no value and one given twice.
pub fn parse(args: &[String], known: &[&str]) -> anyhow::Result<Parsed> {
    let mut positional = Vec::new();
    let mut flags: Vec<(String, String)> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if let Some(name) = arg.strip_prefix("--") {
            ensure!(known.contains(&name), "unknown flag --{name}");
            let value = it
                .next()
                .with_context(|| format!("--{name} needs a value"))?;
            ensure!(
                !flags.iter().any(|(n, _)| n == name),
                "--{name} given twice"
            );
            flags.push((name.to_owned(), value.clone()));
        } else {
            positional.push(arg.clone());
        }
    }
    Ok(Parsed { positional, flags })
}

impl Parsed {
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn required(&self, name: &str) -> anyhow::Result<&str> {
        self.flag(name)
            .with_context(|| format!("--{name} is required"))
    }

    /// The only positional, named `what` in the error.
    pub fn single(&self, what: &str) -> anyhow::Result<&str> {
        ensure!(
            self.positional.len() == 1,
            "expected exactly one {what}, got {:?}",
            self.positional
        );
        Ok(&self.positional[0])
    }
}
