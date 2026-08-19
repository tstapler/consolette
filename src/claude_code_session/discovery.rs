//! Locate and sort Claude Code session transcripts on disk.
//!
//! Complements [`crate::learn::transcript::parse_all_transcripts`], which
//! globs the same `~/.claude/projects/**/*.jsonl` tree but parses each file's
//! contents for correction-pattern mining. This module only stats the files
//! themselves, for commands (`list-sessions`) that need to pick a transcript
//! rather than analyze one.

use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Context, Result};
use glob::glob;

/// One discovered transcript file and the filesystem metadata used to sort it.
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub size_bytes: u64,
}

/// Ordering for [`discover_sessions`] / [`discover_sessions_glob`] results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    RecentFirst,
    OldestFirst,
    LargestFirst,
    SmallestFirst,
}

impl SortBy {
    /// Parses the `--sort` CLI flag's value (`recent`, `oldest`, `largest`,
    /// `smallest`).
    ///
    /// # Errors
    ///
    /// Returns an error if `value` is not one of the recognized sort names.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "recent" => Ok(Self::RecentFirst),
            "oldest" => Ok(Self::OldestFirst),
            "largest" => Ok(Self::LargestFirst),
            "smallest" => Ok(Self::SmallestFirst),
            other => anyhow::bail!(
                "invalid --sort value {other:?}: expected one of recent, oldest, largest, smallest"
            ),
        }
    }
}

/// Locate every session transcript under `~/.claude/projects/**/*.jsonl` and
/// sort the results.
///
/// # Errors
///
/// Returns an error if the `HOME` environment variable is not set.
pub fn discover_sessions(sort_by: SortBy) -> Result<Vec<SessionFile>> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("HOME environment variable not set"))?;
    let pattern = format!("{}/.claude/projects/**/*.jsonl", home.display());
    discover_sessions_glob(&pattern, sort_by)
}

/// Locate transcripts matching an arbitrary glob pattern and sort them.
///
/// Exposed separately so tests can point at fixture directories.
///
/// # Errors
///
/// Returns an error if `pattern` is not a valid glob pattern.
pub fn discover_sessions_glob(pattern: &str, sort_by: SortBy) -> Result<Vec<SessionFile>> {
    let mut sessions = Vec::new();

    for path_result in glob(pattern).map_err(|e| anyhow::anyhow!("glob error: {e}"))? {
        let path = match path_result {
            Ok(path) => path,
            Err(e) => {
                tracing::debug!("glob entry error: {e}");
                continue;
            }
        };
        let metadata =
            fs::metadata(&path).with_context(|| format!("failed to stat {}", path.display()))?;
        sessions.push(SessionFile {
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size_bytes: metadata.len(),
            path,
        });
    }

    sort_sessions(&mut sessions, sort_by);
    Ok(sessions)
}

fn sort_sessions(sessions: &mut [SessionFile], sort_by: SortBy) {
    match sort_by {
        SortBy::RecentFirst => sessions.sort_by_key(|s| std::cmp::Reverse(s.modified)),
        SortBy::OldestFirst => sessions.sort_by_key(|s| s.modified),
        SortBy::LargestFirst => sessions.sort_by_key(|s| std::cmp::Reverse(s.size_bytes)),
        SortBy::SmallestFirst => sessions.sort_by_key(|s| s.size_bytes),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::thread::sleep;
    use std::time::Duration;

    #[test]
    fn sorts_by_recency_and_size() {
        let dir = tempfile::tempdir().unwrap();

        let small = dir.path().join("small.jsonl");
        File::create(&small).unwrap().write_all(b"{}").unwrap();

        sleep(Duration::from_millis(10));

        let big = dir.path().join("big.jsonl");
        File::create(&big)
            .unwrap()
            .write_all(b"{}{}{}{}{}")
            .unwrap();

        let pattern = format!("{}/*.jsonl", dir.path().display());

        let recent = discover_sessions_glob(&pattern, SortBy::RecentFirst).unwrap();
        assert_eq!(recent[0].path, big);
        assert_eq!(recent[1].path, small);

        let oldest = discover_sessions_glob(&pattern, SortBy::OldestFirst).unwrap();
        assert_eq!(oldest[0].path, small);
        assert_eq!(oldest[1].path, big);

        let largest = discover_sessions_glob(&pattern, SortBy::LargestFirst).unwrap();
        assert_eq!(largest[0].path, big);
        assert_eq!(largest[1].path, small);

        let smallest = discover_sessions_glob(&pattern, SortBy::SmallestFirst).unwrap();
        assert_eq!(smallest[0].path, small);
        assert_eq!(smallest[1].path, big);
    }

    #[test]
    fn rejects_unknown_sort_value() {
        assert!(SortBy::parse("bogus").is_err());
    }
}
