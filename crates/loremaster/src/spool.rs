//! Pointers to turns that ended while the daemon was down. They hold no prompt or reply text.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Verb;
use crate::config::create_private_dir;

/// One event to process when the daemon next starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpoolEntry {
    pub agent: String,
    pub verb: Verb,
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
}

/// Appends one entry. A single `O_APPEND` write keeps concurrent hooks from interleaving.
pub fn append(path: &Path, entry: &SpoolEntry) -> io::Result<()> {
    create_private_dir(path.parent().unwrap_or(Path::new(".")))?;
    let mut line = serde_json::to_string(entry)?;
    line.push('\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?
        .write_all(line.as_bytes())
}

/// Reads all entries. A malformed line is logged and skipped.
pub fn read(path: &Path) -> io::Result<Vec<SpoolEntry>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(bytes
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| match serde_json::from_slice(line) {
            Ok(entry) => Some(entry),
            Err(e) => {
                tracing::error!(error = %e, "skipped malformed spool line");
                None
            }
        })
        .collect())
}

/// Leaves only `entries` in the spool, or no spool at all if there are none.
pub fn rewrite(path: &Path, entries: &[SpoolEntry]) -> io::Result<()> {
    if entries.is_empty() {
        return match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap_or(Path::new(".")))?;
    for entry in entries {
        writeln!(temp, "{}", serde_json::to_string(entry)?)?;
    }
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> SpoolEntry {
        SpoolEntry {
            agent: "claude-code".into(),
            verb: Verb::Capture,
            session_id: "s1".into(),
            transcript_path: Some("/t.jsonl".into()),
            cwd: "/w".into(),
        }
    }

    #[test]
    fn line_has_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/spool");
        append(&path, &entry()).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\"agent\":\"claude-code\",\"verb\":\"capture\",\"session_id\":\"s1\",\"transcript_path\":\"/t.jsonl\",\"cwd\":\"/w\"}\n"
        );
    }

    #[test]
    fn entries_read_back_in_order_and_bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        append(&path, &entry()).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"garbage\n")
            .unwrap();
        let mut second = entry();
        second.verb = Verb::SessionEnd;
        second.transcript_path = None;
        append(&path, &second).unwrap();
        assert_eq!(read(&path).unwrap(), [entry(), second]);
    }

    #[test]
    fn missing_spool_reads_empty_and_clears_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        assert!(read(&path).unwrap().is_empty());
        rewrite(&path, &[]).unwrap();
    }

    #[test]
    fn clear_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        append(&path, &entry()).unwrap();
        rewrite(&path, &[]).unwrap();
        assert!(read(&path).unwrap().is_empty());
    }

    #[test]
    fn rewrite_keeps_only_the_given_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        append(&path, &entry()).unwrap();
        let mut kept = entry();
        kept.session_id = "s2".into();
        append(&path, &kept).unwrap();
        rewrite(&path, std::slice::from_ref(&kept)).unwrap();
        assert_eq!(read(&path).unwrap(), [kept]);
    }

    #[test]
    fn invalid_utf8_line_is_skipped_and_others_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spool");
        fs::write(&path, b"\xff\xfe\n").unwrap();
        append(&path, &entry()).unwrap();
        assert_eq!(read(&path).unwrap(), [entry()]);
    }

    #[test]
    fn a_home_created_for_the_spool_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home/spool");
        append(&path, &entry()).unwrap();
        let mode = fs::metadata(dir.path().join("home"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn io_errors_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).is_err());
        assert!(rewrite(dir.path(), &[]).is_err());
        assert!(rewrite(Path::new("/nonexistent/spool"), &[entry()]).is_err());
        assert!(append(dir.path(), &entry()).is_err());
    }
}
