//! The result line `run` prints, and the content check behind its `ok`.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;

use serde::Serialize;

use crate::fixtures::{self, FixtureFile};
use crate::frame;

/// One `run`'s result, printed as one JSON line.
#[derive(Debug, Serialize)]
pub struct RunResult {
    pub candidate: String,
    pub shape: String,
    pub files: usize,
    pub bytes: u64,
    /// Transfers outstanding at once: the fetch candidates only.
    pub in_flight: Option<usize>,
    pub wall_ms: u64,
    pub local_cpu_ms: Option<u64>,
    pub remote_cpu_ms: Option<u64>,
    /// Why a CPU field is `null`, or what makes it less than clean.
    pub cpu_note: Option<String>,
    pub ok: bool,
    /// Why `ok` is false: `hung`, the failure, or the first bad path.
    pub error: Option<String>,
}

/// A writer that compares what is written with `actual`, recording the first
/// offset where they differ (or `actual` runs short).
struct Compare<R> {
    actual: R,
    offset: u64,
    differs: Option<u64>,
    buf: Vec<u8>,
}

impl<R: Read> Write for Compare<R> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.differs.is_none() {
            self.buf.resize(data.len(), 0);
            let n = frame::fill(&mut self.actual, &mut self.buf)?;
            if let Some(i) = data[..n]
                .iter()
                .zip(&self.buf[..n])
                .position(|(want, got)| want != got)
            {
                self.differs = Some(self.offset + i as u64);
            } else if n < data.len() {
                self.differs = Some(self.offset + n as u64);
            }
        }
        self.offset += data.len() as u64;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The offset of the first byte where `actual` differs from `file`'s fixture
/// bytes, ends early, or goes on past them; `None` when it matches.
pub fn first_difference(file: &FixtureFile, actual: impl Read) -> io::Result<Option<u64>> {
    let mut compare = Compare {
        actual,
        offset: 0,
        differs: None,
        buf: Vec::new(),
    };
    fixtures::write_contents(file, &mut compare)?;
    if compare.differs.is_none() {
        let mut extra = [0u8; 1];
        if frame::fill(&mut compare.actual, &mut extra)? > 0 {
            compare.differs = Some(compare.offset);
        }
    }
    Ok(compare.differs)
}

/// Check every manifest file through `open` (its size and a reader, `None`
/// when missing) against the fixture's bytes, reporting the first file that
/// is missing, the wrong size, or different.
pub fn verify<R: Read>(
    files: &[FixtureFile],
    mut open: impl FnMut(&str) -> io::Result<Option<(u64, R)>>,
) -> Result<(), String> {
    for file in files {
        let opened = open(&file.rel).map_err(|e| format!("{}: {e}", file.rel))?;
        let Some((len, reader)) = opened else {
            return Err(format!("missing {}", file.rel));
        };
        if len != file.len {
            return Err(format!(
                "{}: {len} bytes on disk, expected {}",
                file.rel, file.len
            ));
        }
        match first_difference(file, reader) {
            Ok(None) => {}
            Ok(Some(at)) => {
                return Err(format!(
                    "{}: contents differ from the fixture at byte {at}",
                    file.rel
                ));
            }
            Err(err) => return Err(format!("{}: reading: {err}", file.rel)),
        }
    }
    Ok(())
}

/// [`verify`] against the files under `root`.
pub fn verify_dir(files: &[FixtureFile], root: &Path) -> Result<(), String> {
    verify(files, |rel| {
        let file = match File::open(root.join(rel)) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Ok(None);
        }
        Ok(Some((meta.len(), BufReader::new(file))))
    })
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code")]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::fixtures::Scale;

    #[test]
    fn the_result_line_has_exactly_the_documented_fields() {
        let result = RunResult {
            candidate: "fetch-n".to_owned(),
            shape: "small-files".to_owned(),
            files: 50,
            bytes: 1234,
            in_flight: Some(4),
            wall_ms: 900,
            local_cpu_ms: Some(15),
            remote_cpu_ms: None,
            cpu_note: Some("daemon not idle".to_owned()),
            ok: true,
            error: None,
        };
        let value = serde_json::to_value(&result).expect("serialize");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "bytes",
                "candidate",
                "cpu_note",
                "error",
                "files",
                "in_flight",
                "local_cpu_ms",
                "ok",
                "remote_cpu_ms",
                "shape",
                "wall_ms"
            ]
        );
        assert_eq!(object["candidate"], "fetch-n");
        assert_eq!(object["in_flight"], 4);
        assert_eq!(object["local_cpu_ms"], 15);
        assert!(object["remote_cpu_ms"].is_null());
        assert_eq!(object["cpu_note"], "daemon not idle");
        assert_eq!(object["ok"], true);
    }

    fn fixture_bytes(files: &[FixtureFile]) -> HashMap<String, Vec<u8>> {
        files
            .iter()
            .map(|f| {
                let mut buf = Vec::new();
                fixtures::write_contents(f, &mut buf).expect("write to a Vec");
                (f.rel.clone(), buf)
            })
            .collect()
    }

    fn check(files: &[FixtureFile], disk: &HashMap<String, Vec<u8>>) -> Result<(), String> {
        verify(files, |rel| {
            Ok(disk
                .get(rel)
                .map(|bytes| (bytes.len() as u64, bytes.as_slice())))
        })
    }

    #[test]
    fn the_verifier_compares_contents_and_reports_the_first_bad_file() {
        let files = fixtures::manifest("small-files", Scale::Small).expect("manifest");
        let files = &files[..3];
        let mut disk = fixture_bytes(files);
        assert_eq!(check(files, &disk), Ok(()));

        // Same size, one byte changed in the second file.
        disk.get_mut(&files[1].rel).expect("second")[100] ^= 0x20;
        assert_eq!(
            check(files, &disk),
            Err(format!(
                "{}: contents differ from the fixture at byte 100",
                files[1].rel
            ))
        );

        disk = fixture_bytes(files);
        disk.get_mut(&files[0].rel).expect("first").pop();
        assert_eq!(
            check(files, &disk),
            Err(format!(
                "{}: {} bytes on disk, expected {}",
                files[0].rel,
                files[0].len - 1,
                files[0].len
            ))
        );

        disk = fixture_bytes(files);
        disk.remove(&files[2].rel);
        assert_eq!(
            check(files, &disk),
            Err(format!("missing {}", files[2].rel))
        );
    }

    #[test]
    fn a_short_or_long_stream_differs_where_it_stops_matching() {
        let files = fixtures::manifest("html-media", Scale::Small).expect("manifest");
        let image = &files[1];
        let mut bytes = Vec::new();
        fixtures::write_contents(image, &mut bytes).expect("bytes");
        assert_eq!(
            first_difference(image, bytes.as_slice()).expect("read"),
            None
        );
        assert_eq!(
            first_difference(image, &bytes[..1000]).expect("read"),
            Some(1000)
        );
        bytes.push(0);
        assert_eq!(
            first_difference(image, bytes.as_slice()).expect("read"),
            Some(image.len)
        );
    }
}
