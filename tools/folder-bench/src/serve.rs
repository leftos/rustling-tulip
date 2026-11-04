//! `serve-tar`: runs on the builder, packs a folder with the system tar
//! (piped through the zstd CLI for a zstd codec) and writes it to stdout raw
//! or as base64 JSON-line frames.
//!
//! The zstd CLI, not bsdtar's own `--zstd`: bsdtar 3.8.9 writing `--zstd` to
//! `-f -` drops the last partial block, so the stream it writes is truncated.

use std::io::{self, BufWriter, Read, Write as _};
use std::process::{Child, Command, ExitCode, Stdio};

use anyhow::{Context as _, bail, ensure};

use crate::{args, cpu, frame, remote};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    None,
    Zstd1,
    Zstd3,
}

impl Codec {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "none" => Self::None,
            "zstd1" => Self::Zstd1,
            "zstd3" => Self::Zstd3,
            other => bail!("--codec takes none, zstd1 or zstd3, not {other:?}"),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Zstd1 => "zstd1",
            Self::Zstd3 => "zstd3",
        }
    }

    /// The zstd level, `None` for an uncompressed stream.
    pub fn level(self) -> Option<u8> {
        match self {
            Self::None => None,
            Self::Zstd1 => Some(1),
            Self::Zstd3 => Some(3),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Base64,
    Raw,
}

impl FrameKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Base64 => "base64",
            Self::Raw => "raw",
        }
    }

    pub fn parse(value: &str) -> anyhow::Result<Self> {
        Ok(match value {
            "base64" => Self::Base64,
            "raw" => Self::Raw,
            other => bail!("--frame takes base64 or raw, not {other:?}"),
        })
    }
}

pub fn command(rest: &[String]) -> anyhow::Result<ExitCode> {
    let parsed = args::parse(rest, &["codec", "frame", "zstd"])?;
    let dir = parsed.single("folder")?;
    let codec = Codec::parse(parsed.required("codec")?)?;
    let kind = FrameKind::parse(parsed.required("frame")?)?;
    let zstd_exe = parsed.flag("zstd").unwrap_or(remote::REMOTE_ZSTD);

    let mut tar = Command::new(remote::TAR)
        .args(["-c", "-f", "-", "-C", dir, "."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting {}", remote::TAR))?;
    let tar_out = tar.stdout.take().context("tar's stdout")?;
    let (mut packed, zstd): (Box<dyn Read>, Option<Child>) = match codec.level() {
        None => (Box::new(tar_out), None),
        Some(level) => {
            let mut zstd = Command::new(zstd_exe)
                .arg(format!("-{level}"))
                .args(["-c", "-q"])
                .stdin(Stdio::from(tar_out))
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("starting {zstd_exe}"))?;
            let out = zstd.stdout.take().context("zstd's stdout")?;
            (Box::new(out), Some(zstd))
        }
    };
    let mut out = BufWriter::with_capacity(2 * frame::CHUNK, io::stdout().lock());
    match kind {
        FrameKind::Raw => {
            io::copy(&mut packed, &mut out).context("copying the packed stream")?;
        }
        FrameKind::Base64 => {
            frame::encode(&mut packed, &mut out, frame::CHUNK)
                .context("framing the packed stream")?;
        }
    }
    let status = tar.wait().context("waiting for tar")?;
    ensure!(status.success(), "tar exited with {status}");
    let mut zstd_cpu = None;
    if let Some(mut zstd) = zstd {
        let status = zstd.wait().context("waiting for zstd")?;
        ensure!(status.success(), "zstd exited with {status}");
        zstd_cpu = Some(cpu::child_cpu_ms(&zstd));
    }
    let cpu_ms = pipeline_cpu(cpu::own_cpu_ms(), cpu::child_cpu_ms(&tar), zstd_cpu);
    match kind {
        FrameKind::Base64 => frame::write_end(&mut out, cpu_ms)?,
        // Raw bytes leave no room for a trailer; the CPU line goes to stderr.
        FrameKind::Raw => {
            out.flush()?;
            let mut err = io::stderr().lock();
            serde_json::to_writer(&mut err, &frame::Frame::End { cpu_ms })?;
            writeln!(err)?;
        }
    }
    out.flush()?;
    Ok(ExitCode::SUCCESS)
}

/// The pipeline's CPU: this process, tar and, when the codec runs one, zstd
/// (`None` when there is none); unknown when any present part is.
#[expect(
    clippy::option_option,
    reason = "a codec with no zstd differs from a zstd whose CPU could not be read"
)]
fn pipeline_cpu(own: Option<u64>, tar: Option<u64>, zstd: Option<Option<u64>>) -> Option<u64> {
    cpu::sum([own, tar].into_iter().chain(zstd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_zstd_adds_nothing_and_an_unread_one_is_unknown() {
        assert_eq!(pipeline_cpu(Some(5), Some(7), None), Some(12));
        assert_eq!(pipeline_cpu(Some(5), Some(7), Some(Some(3))), Some(15));
        assert_eq!(pipeline_cpu(Some(5), Some(7), Some(None)), None);
        assert_eq!(pipeline_cpu(None, Some(7), None), None);
    }
}
