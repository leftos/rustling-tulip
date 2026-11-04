//! The deterministic fixture folders: one manifest per shape (names, sizes and
//! a seed per file) and the bytes each file holds, from a fixed-seed xorshift.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context as _, bail};

use crate::args;

pub const SHAPES: [&str; 3] = ["small-files", "large-binaries", "html-media"];

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
const BLOCK: usize = 64 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
const WORDS: [&str; 16] = [
    "folder", "bench", "remote", "host", "fetch", "chunk", "stream", "daemon", "session", "tulip",
    "rustling", "builder", "tailnet", "archive", "byte", "frame",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scale {
    Small,
    Full,
}

impl Scale {
    /// `small` or `full`; absent is `full`.
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value {
            None | Some("full") => Ok(Self::Full),
            Some("small") => Ok(Self::Small),
            Some(other) => bail!("--scale takes small or full, not {other:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text { seed: u64 },
    Binary { seed: u64 },
    Image { seed: u64 },
    Literal(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureFile {
    /// Path relative to the shape's folder, `/`-separated.
    pub rel: String,
    pub len: u64,
    pub content: Content,
}

/// A small xorshift64 generator: deterministic, no crate.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Every file of `shape` at `scale`, in a fixed order.
pub fn manifest(shape: &str, scale: Scale) -> anyhow::Result<Vec<FixtureFile>> {
    let small = scale == Scale::Small;
    Ok(match shape {
        "small-files" => small_files(if small { 50 } else { 2000 }),
        "large-binaries" => large_binaries(if small { MIB } else { 64 * MIB }),
        "html-media" => html_media(if small { 3 } else { 20 }),
        other => bail!("unknown shape {other:?}; the shapes are {SHAPES:?}"),
    })
}

fn small_files(count: usize) -> Vec<FixtureFile> {
    let mut rng = XorShift::new(SEED ^ 1);
    (0..count)
        .map(|i| {
            let len = KIB + rng.next() % (19 * KIB + 1);
            FixtureFile {
                rel: format!("d{:02}/s{}/file-{i:04}.txt", i % 20, (i / 20) % 5),
                len,
                content: Content::Text { seed: rng.next() },
            }
        })
        .collect()
}

fn large_binaries(len: u64) -> Vec<FixtureFile> {
    let mut rng = XorShift::new(SEED ^ 2);
    (0..3)
        .map(|i| FixtureFile {
            rel: format!("blob-{i}.bin"),
            len,
            content: Content::Binary { seed: rng.next() },
        })
        .collect()
}

fn html_media(images: usize) -> Vec<FixtureFile> {
    let mut rng = XorShift::new(SEED ^ 3);
    let mut files: Vec<FixtureFile> = (0..images)
        .map(|i| FixtureFile {
            rel: format!("media/img-{i:02}.png"),
            len: 256 * KIB,
            content: Content::Image { seed: rng.next() },
        })
        .collect();
    let mut html = String::from(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>folder-bench</title></head><body>\n",
    );
    for file in &files {
        for piece in [
            "<figure><img src=\"",
            &file.rel,
            "\" alt=\"",
            &file.rel,
            "\"></figure>\n",
        ] {
            html.push_str(piece);
        }
    }
    html.push_str("</body></html>\n");
    files.insert(
        0,
        FixtureFile {
            rel: "index.html".to_owned(),
            len: html.len() as u64,
            content: Content::Literal(html),
        },
    );
    files
}

/// Write `file`'s bytes, exactly `file.len` of them.
pub fn write_contents(file: &FixtureFile, out: &mut impl Write) -> io::Result<()> {
    match &file.content {
        Content::Literal(text) => out.write_all(text.as_bytes()),
        Content::Text { seed } => out.write_all(&text_bytes(*seed, file.len)),
        Content::Binary { seed } => write_random(*seed, file.len, &[], out),
        Content::Image { seed } => write_random(*seed, file.len, PNG_MAGIC, out),
    }
}

fn text_bytes(seed: u64, len: u64) -> Vec<u8> {
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let mut rng = XorShift::new(seed);
    let mut text = Vec::with_capacity(len + 16);
    let mut words_on_line = 0;
    while text.len() < len {
        let pick = usize::try_from(rng.next() % WORDS.len() as u64).unwrap_or(0);
        text.extend_from_slice(WORDS[pick].as_bytes());
        words_on_line += 1;
        if words_on_line == 12 {
            text.push(b'\n');
            words_on_line = 0;
        } else {
            text.push(b' ');
        }
    }
    text.truncate(len);
    text
}

fn write_random(seed: u64, len: u64, prefix: &[u8], out: &mut impl Write) -> io::Result<()> {
    let mut rng = XorShift::new(seed);
    let mut pending = prefix;
    let mut block = Vec::with_capacity(BLOCK);
    let mut written: u64 = 0;
    while written < len {
        block.clear();
        let want = usize::try_from((len - written).min(BLOCK as u64)).unwrap_or(BLOCK);
        let take = pending.len().min(want);
        block.extend_from_slice(&pending[..take]);
        pending = &pending[take..];
        while block.len() < want {
            let word = rng.next().to_le_bytes();
            let n = (want - block.len()).min(word.len());
            block.extend_from_slice(&word[..n]);
        }
        out.write_all(&block)?;
        written += want as u64;
    }
    Ok(())
}

/// Write every shape's files under `dir/<shape>/`, returning each shape's
/// file count and total bytes.
pub fn make(dir: &Path, scale: Scale) -> anyhow::Result<Vec<(&'static str, usize, u64)>> {
    let mut totals = Vec::new();
    for shape in SHAPES {
        let files = manifest(shape, scale)?;
        let root = dir.join(shape);
        for file in &files {
            let path = root.join(&file.rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            let handle =
                fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
            let mut out = BufWriter::new(handle);
            write_contents(file, &mut out)
                .and_then(|()| out.flush())
                .with_context(|| format!("writing {}", path.display()))?;
        }
        totals.push((shape, files.len(), files.iter().map(|f| f.len).sum()));
    }
    Ok(totals)
}

pub fn command(rest: &[String]) -> anyhow::Result<ExitCode> {
    let parsed = args::parse(rest, &["scale"])?;
    let dir = parsed.single("fixtures folder")?;
    let scale = Scale::parse(parsed.flag("scale"))?;
    let totals = make(Path::new(dir), scale)?;
    let mut out = io::stdout().lock();
    for (shape, files, bytes) in totals {
        let line = serde_json::json!({ "shape": shape, "files": files, "bytes": bytes });
        writeln!(out, "{line}")?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    fn all_bytes(files: &[FixtureFile]) -> Vec<Vec<u8>> {
        files
            .iter()
            .map(|f| {
                let mut buf = Vec::new();
                write_contents(f, &mut buf).expect("write to a Vec");
                buf
            })
            .collect()
    }

    #[test]
    fn same_seed_gives_same_names_sizes_and_bytes() {
        for shape in SHAPES {
            let first = manifest(shape, Scale::Small).expect("manifest");
            let second = manifest(shape, Scale::Small).expect("manifest");
            assert_eq!(first, second, "{shape}");
            assert_eq!(all_bytes(&first), all_bytes(&second), "{shape}");
        }
    }

    #[test]
    fn every_file_holds_exactly_its_manifest_length() {
        for shape in SHAPES {
            let files = manifest(shape, Scale::Small).expect("manifest");
            for (file, bytes) in files.iter().zip(all_bytes(&files)) {
                assert_eq!(bytes.len() as u64, file.len, "{}", file.rel);
            }
        }
    }

    #[test]
    fn small_scale_counts_and_sizes() {
        let small = manifest("small-files", Scale::Small).expect("manifest");
        assert_eq!(small.len(), 50);
        assert!(small.iter().all(|f| (KIB..=20 * KIB).contains(&f.len)));
        assert!(small.iter().all(|f| f.rel.matches('/').count() == 2));

        let large = manifest("large-binaries", Scale::Small).expect("manifest");
        assert_eq!(large.len(), 3);
        assert!(large.iter().all(|f| f.len == MIB));

        let html = manifest("html-media", Scale::Small).expect("manifest");
        assert_eq!(html.len(), 4);
        assert_eq!(html[0].rel, "index.html");
        let Content::Literal(page) = &html[0].content else {
            unreachable!("index.html is literal text");
        };
        for image in &html[1..] {
            assert_eq!(image.len, 256 * KIB);
            assert!(page.contains(&format!("src=\"{}\"", image.rel)));
        }
    }

    #[test]
    fn full_scale_counts() {
        assert_eq!(manifest("small-files", Scale::Full).expect("m").len(), 2000);
        let large = manifest("large-binaries", Scale::Full).expect("m");
        assert!(large.iter().all(|f| f.len == 64 * MIB));
        assert_eq!(manifest("html-media", Scale::Full).expect("m").len(), 21);
    }

    #[test]
    fn images_start_with_the_png_signature_and_text_is_text() {
        let html = manifest("html-media", Scale::Small).expect("manifest");
        let bytes = all_bytes(&html);
        assert!(bytes[1].starts_with(PNG_MAGIC));
        let small = manifest("small-files", Scale::Small).expect("manifest");
        let text = &all_bytes(&small[..1])[0];
        assert!(
            text.iter()
                .all(|b| b.is_ascii_lowercase() || *b == b' ' || *b == b'\n')
        );
    }

    #[test]
    fn an_unknown_shape_or_scale_is_refused() {
        assert!(manifest("nope", Scale::Small).is_err());
        assert!(Scale::parse(Some("huge")).is_err());
        assert_eq!(Scale::parse(None).expect("default"), Scale::Full);
    }
}
