//! The base64 JSON-line framing `serve-tar --frame base64` writes: one
//! `{"type":"chunk","data":"<base64>"}` line per 256 KiB of input, then
//! `{"type":"end","cpu_ms":<n>}`. It mimics `FetchFileChunk`'s cost (the same
//! chunk size and encoding, serialized through serde); it is not that type.

use std::io::{self, BufRead, Read, Write};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// The daemon's `FetchFile` chunk size.
pub const CHUNK: usize = 256 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Chunk { data: String },
    End { cpu_ms: Option<u64> },
}

/// Fill `buf` from `input` until it is full or `input` ends; the count read.
pub fn fill(input: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match input.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(filled)
}

/// Write `input` as chunk frames of `chunk` bytes each (the last shorter),
/// returning the number of frames. Writes no end frame.
pub fn encode(input: &mut impl Read, out: &mut impl Write, chunk: usize) -> io::Result<u64> {
    let mut buf = vec![0u8; chunk];
    let mut frames = 0;
    loop {
        let n = fill(input, &mut buf)?;
        if n == 0 {
            return Ok(frames);
        }
        let frame = Frame::Chunk {
            data: base64::engine::general_purpose::STANDARD.encode(&buf[..n]),
        };
        serde_json::to_writer(&mut *out, &frame)?;
        out.write_all(b"\n")?;
        frames += 1;
    }
}

/// Write the end frame carrying the sender's CPU time.
pub fn write_end(out: &mut impl Write, cpu_ms: Option<u64>) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &Frame::End { cpu_ms })?;
    out.write_all(b"\n")
}

/// Decode frames from `input` into `out` as they arrive, returning the end
/// frame's CPU time. A stream that stops before its end frame is an error.
pub fn decode(input: &mut impl BufRead, out: &mut impl Write) -> anyhow::Result<Option<u64>> {
    let mut line = String::new();
    let mut bytes = Vec::with_capacity(CHUNK);
    loop {
        line.clear();
        if input.read_line(&mut line).context("reading a frame")? == 0 {
            bail!("the stream ended before its end frame");
        }
        match serde_json::from_str::<Frame>(&line).context("parsing a frame")? {
            Frame::Chunk { data } => {
                bytes.clear();
                base64::engine::general_purpose::STANDARD
                    .decode_vec(data.as_bytes(), &mut bytes)
                    .context("decoding a chunk")?;
                out.write_all(&bytes).context("writing a chunk")?;
            }
            Frame::End { cpu_ms } => return Ok(cpu_ms),
        }
    }
}

/// What an end frame reported about the sender's CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reported {
    /// No end frame arrived.
    Missing,
    /// The end frame came, but the sender could not read its CPU.
    Unknown,
    Ms(u64),
}

impl Reported {
    pub fn from_end(cpu_ms: Option<u64>) -> Self {
        cpu_ms.map_or(Self::Unknown, Self::Ms)
    }
}

/// The CPU the first end-frame line in `text` (a child's stderr) carries.
pub fn end_line(text: &str) -> Reported {
    text.lines()
        .find_map(|line| match serde_json::from_str::<Frame>(line.trim()) {
            Ok(Frame::End { cpu_ms }) => Some(Reported::from_end(cpu_ms)),
            _ => None,
        })
        .unwrap_or(Reported::Missing)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn the_end_line_is_found_among_other_stderr_lines() {
        assert_eq!(
            end_line("warning: x\r\n{\"type\":\"end\",\"cpu_ms\":31}\r\n"),
            Reported::Ms(31)
        );
        assert_eq!(
            end_line("{\"type\":\"end\",\"cpu_ms\":null}"),
            Reported::Unknown
        );
        assert_eq!(
            end_line("rustling-tulipd bridge: failed"),
            Reported::Missing
        );
        assert_eq!(end_line(""), Reported::Missing);
    }

    fn input(len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| u8::try_from((i * 31 + i / 7) % 251).expect("below 251"))
            .collect()
    }

    #[test]
    fn chunks_round_trip_and_the_last_is_shorter() {
        let data = input(2 * CHUNK + 1000);
        let mut wire = Vec::new();
        let frames = encode(&mut data.as_slice(), &mut wire, CHUNK).expect("encode");
        assert_eq!(frames, 3);
        write_end(&mut wire, Some(42)).expect("end");

        let lens: Vec<usize> = wire
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .filter_map(
                |l| match serde_json::from_slice::<Frame>(l).expect("frame") {
                    Frame::Chunk { data } => Some(
                        base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .expect("base64")
                            .len(),
                    ),
                    Frame::End { .. } => None,
                },
            )
            .collect();
        assert_eq!(lens, [CHUNK, CHUNK, 1000]);

        let mut out = Vec::new();
        let cpu = decode(&mut wire.as_slice(), &mut out).expect("decode");
        assert_eq!(cpu, Some(42));
        assert_eq!(out, data);
    }

    #[test]
    fn the_wire_lines_have_the_documented_shape() {
        let mut wire = Vec::new();
        encode(&mut b"hi".as_slice(), &mut wire, CHUNK).expect("encode");
        write_end(&mut wire, Some(7)).expect("end");
        let text = String::from_utf8(wire).expect("utf-8");
        assert_eq!(
            text,
            "{\"type\":\"chunk\",\"data\":\"aGk=\"}\n{\"type\":\"end\",\"cpu_ms\":7}\n"
        );
    }

    #[test]
    fn empty_input_is_no_chunk_and_a_stream_without_end_fails() {
        let mut wire = Vec::new();
        assert_eq!(
            encode(&mut [].as_slice(), &mut wire, CHUNK).expect("encode"),
            0
        );
        let mut out = Vec::new();
        assert!(decode(&mut wire.as_slice(), &mut out).is_err());
    }
}
