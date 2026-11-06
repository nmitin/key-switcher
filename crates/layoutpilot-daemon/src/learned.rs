//! Persistent learning journal.
//!
//! Learning events ([`Learning`]) are appended to a small TSV file and replayed
//! at startup, so a word kept after an undo stays kept across restarts. Only
//! words involved in a correction the user reacted to are stored — never the
//! typed text around them.
//!
//! Location: `$STATE_DIRECTORY` (set by systemd's `StateDirectory=`), else
//! `$XDG_STATE_HOME/layoutpilot`, else `~/.local/state/layoutpilot`. The file
//! can be edited or deleted by hand; unknown lines are ignored.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use layoutpilot_core::Learning;

const FILE_NAME: &str = "learned.tsv";

/// Journal path, if a home or state directory is known.
pub(crate) fn path() -> Option<PathBuf> {
    let dir = env::var_os("STATE_DIRECTORY")
        .and_then(|dirs| env::split_paths(&dirs).next())
        .or_else(|| env::var_os("XDG_STATE_HOME").map(|dir| PathBuf::from(dir).join("layoutpilot")))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state/layoutpilot"))
        })?;
    Some(dir.join(FILE_NAME))
}

/// Read the journal; a missing file is an empty journal.
pub(crate) fn load(path: &Path) -> io::Result<Vec<Learning>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(parse(&text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Append events, creating the directory and a private (0600) file.
pub(crate) fn append(path: &Path, events: &[Learning]) -> io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    let mut text = String::new();
    for event in events {
        if let Some(line) = format_line(event) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    file.write_all(text.as_bytes())
}

fn format_line(event: &Learning) -> Option<String> {
    let (kind, word) = match event {
        Learning::Keep(word) => ("keep", word),
        Learning::Accept(word) => ("accept", word),
        Learning::Retract(word) => ("retract", word),
    };
    // Words come from the engine's word cores; refuse anything that would
    // break the line format.
    if word.is_empty() || word.chars().any(char::is_control) {
        return None;
    }
    Some(format!("{kind}\t{word}"))
}

fn parse(text: &str) -> Vec<Learning> {
    text.lines()
        .filter_map(|line| {
            let (kind, word) = line.split_once('\t')?;
            let word = word.trim();
            if word.is_empty() {
                return None;
            }
            let word = word.to_owned();
            match kind.trim() {
                "keep" => Some(Learning::Keep(word)),
                "accept" => Some(Learning::Accept(word)),
                "retract" => Some(Learning::Retract(word)),
                _ => None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let events = vec![
            Learning::Keep("ghbd".to_owned()),
            Learning::Accept("привет".to_owned()),
            Learning::Retract("привет".to_owned()),
        ];
        let text: String = events
            .iter()
            .filter_map(format_line)
            .map(|line| line + "\n")
            .collect();
        assert_eq!(text, "keep\tghbd\naccept\tпривет\nretract\tпривет\n");
        assert_eq!(parse(&text), events);
    }

    #[test]
    fn junk_lines_are_ignored() {
        let text = "# comment\nkeep\nunknown\tword\nkeep\t\n\nkeep\tok\n";
        assert_eq!(parse(text), vec![Learning::Keep("ok".to_owned())]);
    }

    #[test]
    fn control_characters_are_not_written() {
        assert_eq!(format_line(&Learning::Keep("a\nb".to_owned())), None);
    }

    #[test]
    fn append_and_load() {
        let dir = env::temp_dir().join(format!("layoutpilot-learned-{}", std::process::id()));
        let path = dir.join(FILE_NAME);
        let _ = fs::remove_file(&path);
        assert_eq!(load(&path).unwrap(), Vec::new());
        append(&path, &[Learning::Keep("one".to_owned())]).unwrap();
        append(&path, &[Learning::Accept("two".to_owned())]).unwrap();
        assert_eq!(
            load(&path).unwrap(),
            vec![
                Learning::Keep("one".to_owned()),
                Learning::Accept("two".to_owned())
            ]
        );
        let mode = fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
