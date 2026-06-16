//! Generic incremental file tailer.
//!
//! Service-agnostic: given a path and the byte offset we last committed, it
//! reads only the newly-appended *complete* lines and invokes a callback for
//! each. Partial trailing lines (a writer mid-flush) are left unconsumed until
//! their terminating newline arrives. Truncation/rotation (file shorter than our
//! offset) resets the offset to 0 and re-reads from the start.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Outcome of scanning one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    /// New committed offset (only advances past complete lines).
    pub new_offset: u64,
    /// True if truncation/rotation was detected and the offset was reset to 0
    /// before reading.
    pub reset: bool,
}

/// Read newly-appended complete lines from `path` starting at `start_offset`,
/// calling `on_line` once per non-empty line in file order.
///
/// Returns the offset to persist. The callback sees only whole lines; the
/// returned offset never moves past an unterminated trailing line.
pub fn scan_file<F>(
    path: &Path,
    start_offset: u64,
    mut on_line: F,
) -> std::io::Result<ScanResult>
where
    F: FnMut(&str),
{
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        // File vanished between event and scan (deleted/rotated): nothing to do.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ScanResult {
                new_offset: start_offset,
                reset: false,
            });
        }
        Err(e) => return Err(e),
    };

    // Truncation / replace-in-place: file is shorter than where we left off.
    let (start, reset) = if len < start_offset {
        (0, true)
    } else {
        (start_offset, false)
    };

    if len == start {
        return Ok(ScanResult {
            new_offset: start,
            reset,
        });
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.take(len - start).read_to_end(&mut buf)?;

    // Only consume up to (and including) the last newline; bytes after it are an
    // incomplete line that we'll pick up on a later scan.
    let last_nl = match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => i,
        None => {
            // No complete line yet — do not advance.
            return Ok(ScanResult {
                new_offset: start,
                reset,
            });
        }
    };

    let complete = &buf[..=last_nl];
    // Lossily decode so a stray non-UTF8 byte can't abort the whole batch; the
    // per-line parser tolerates the rare malformed line.
    let text = String::from_utf8_lossy(complete);
    for line in text.split('\n') {
        if !line.is_empty() {
            on_line(line);
        }
    }

    Ok(ScanResult {
        new_offset: start + (last_nl as u64) + 1,
        reset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn collect(path: &Path, offset: u64) -> (Vec<String>, ScanResult) {
        let mut lines = Vec::new();
        let res = scan_file(path, offset, |l| lines.push(l.to_string())).unwrap();
        (lines, res)
    }

    #[test]
    fn reads_complete_lines_and_advances_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "a\nb\n").unwrap();

        let (lines, res) = collect(&p, 0);
        assert_eq!(lines, vec!["a", "b"]);
        assert_eq!(res.new_offset, 4);
        assert!(!res.reset);

        // Re-scan from the committed offset: nothing new.
        let (lines2, res2) = collect(&p, res.new_offset);
        assert!(lines2.is_empty());
        assert_eq!(res2.new_offset, 4);
    }

    #[test]
    fn does_not_consume_partial_trailing_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "a\npar").unwrap();

        let (lines, res) = collect(&p, 0);
        assert_eq!(lines, vec!["a"]);
        assert_eq!(res.new_offset, 2); // stopped after "a\n", left "par" unconsumed

        // Writer completes the line; the rest is now read exactly once.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "tial\n").unwrap();
        let (lines2, res2) = collect(&p, res.new_offset);
        assert_eq!(lines2, vec!["partial"]);
        assert_eq!(res2.new_offset, 10); // "a\npartial\n"
    }

    #[test]
    fn resets_on_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\n").unwrap();
        let (_l, res) = collect(&p, 0);
        assert_eq!(res.new_offset, 10);

        // File replaced with a shorter one — offset now past EOF.
        std::fs::write(&p, "x\n").unwrap();
        let (lines, res2) = collect(&p, res.new_offset);
        assert!(res2.reset);
        assert_eq!(lines, vec!["x"]);
        assert_eq!(res2.new_offset, 2);
    }

    #[test]
    fn missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.jsonl");
        let (lines, res) = collect(&p, 5);
        assert!(lines.is_empty());
        assert_eq!(res.new_offset, 5);
    }
}
