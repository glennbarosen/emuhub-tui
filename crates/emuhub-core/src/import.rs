//! Local-filesystem side of the ROM import feature: finding candidate ROM
//! files on the user's machine, parsing a terminal's drag-and-drop paste, and
//! turning a selection into a pure upload plan. Mirrors `cascade.rs`'s
//! "plan first, execute later" shape — the actual SFTP upload lives in
//! `transport.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::consoles;
use crate::error::Result;
use crate::scan::ROMS_ROOT;

/// A local file that looks like a ROM: found under the configured import
/// folder, or dropped/pasted onto the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    pub local_path: PathBuf,
    pub filename: String,
    pub size: u64,
    /// Best-guess console folder from the extension, when unambiguous — see
    /// `consoles::console_for_extension`.
    pub suggested: Option<&'static str>,
}

/// Walks `dir` two levels deep (matching the device scan's own `-maxdepth 2`,
/// so a ROM sitting in an extracted subfolder is still found) and returns
/// every file whose extension `consoles::is_rom_extension` recognizes,
/// newest-modified first — "I just downloaded this" is the case being served.
///
/// Propagates an error only for `dir` itself being unreadable (missing import
/// folder vs. an existing-but-empty one are different messages in the UI); a
/// broken subfolder one level down is skipped rather than failing the whole
/// scan.
pub fn scan_local_dir(dir: &Path) -> Result<Vec<ImportCandidate>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        visit_entry(&entry, 2, &mut found);
    }
    found.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    Ok(found.into_iter().map(|(candidate, _)| candidate).collect())
}

fn visit_entry(entry: &fs::DirEntry, depth: u8, out: &mut Vec<(ImportCandidate, SystemTime)>) {
    let Ok(file_type) = entry.file_type() else { return };
    let path = entry.path();

    if file_type.is_dir() {
        if depth > 1 {
            if let Ok(children) = fs::read_dir(&path) {
                for child in children.flatten() {
                    visit_entry(&child, depth - 1, out);
                }
            }
        }
        return;
    }
    if !file_type.is_file() {
        return;
    }
    let Some(candidate) = candidate_from_path(&path) else { return };
    let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    out.push((candidate, modified));
}

/// Turns a single local path — from a directory scan or a dropped/pasted
/// path — into an `ImportCandidate`, or `None` if it doesn't look like a ROM:
/// not a file, no filename, a non-UTF-8 filename, a dotfile, or an extension
/// the device scanner wouldn't recognize either.
pub fn candidate_from_path(path: &Path) -> Option<ImportCandidate> {
    let filename = path.file_name()?.to_str()?.to_string();
    if filename.is_empty() || filename.starts_with('.') {
        return None;
    }
    let extension = filename.rsplit_once('.').map(|(_, ext)| ext)?;
    if !consoles::is_rom_extension(extension) {
        return None;
    }
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let suggested = consoles::console_for_extension(extension).map(|c| c.folder);
    Some(ImportCandidate { local_path: path.to_path_buf(), filename, size: metadata.len(), suggested })
}

/// Turns whatever a terminal pastes on a file drop into local paths.
///
/// Terminals disagree on the exact shape: a bare path, a path wrapped in
/// single or double quotes, backslash-escaped spaces, or a `file://` URI with
/// the path percent-encoded — and a multi-file drop is either one line per
/// file or one line with paths space-separated. This normalizes all of those
/// forms; it does not touch the filesystem, so an unresolvable or
/// nonexistent path is still returned; `candidate_from_path` is what rejects
/// it.
pub fn parse_dropped_paths(text: &str) -> Vec<PathBuf> {
    text.lines()
        .flat_map(split_shell_words)
        .filter(|s| !s.is_empty())
        .map(|token| PathBuf::from(decode_token(&token)))
        .collect()
}

/// A minimal shell-style word splitter: a single or double quote groups a run
/// of characters (including spaces) into one word, and a backslash escapes
/// the character that follows it. Not a full shell grammar — just enough to
/// recover the paths a terminal's drag-and-drop paste actually produces.
fn split_shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                        in_word = true;
                    }
                }
                c if c.is_whitespace() => {
                    if in_word {
                        words.push(std::mem::take(&mut current));
                        in_word = false;
                    }
                }
                c => {
                    current.push(c);
                    in_word = true;
                }
            },
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

/// Strips a `file://` prefix, if present, and percent-decodes the rest.
fn decode_token(token: &str) -> String {
    percent_decode(token.strip_prefix("file://").unwrap_or(token))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One file staged for upload: a local candidate paired with its resolved
/// destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportJob {
    pub local_path: PathBuf,
    pub filename: String,
    pub console_folder: String,
    pub remote_path: String,
    pub size: u64,
}

/// A pure plan for an import batch, built before any SFTP I/O — same shape as
/// `cascade::DeletePlan`/`RenamePlan`, so the confirm step can show exactly
/// where every file will land before anything uploads.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportPlan {
    pub jobs: Vec<ImportJob>,
}

/// Builds an `ImportPlan` from `(candidate, target console folder)` pairs —
/// one per row the user checked in the import overlay, with whatever console
/// they left selected or cycled to for that row.
pub fn plan(selections: &[(ImportCandidate, &str)]) -> ImportPlan {
    let jobs = selections
        .iter()
        .map(|(candidate, console_folder)| ImportJob {
            local_path: candidate.local_path.clone(),
            filename: candidate.filename.clone(),
            console_folder: console_folder.to_string(),
            remote_path: format!("{ROMS_ROOT}/{console_folder}/{}", candidate.filename),
            size: candidate.size,
        })
        .collect();
    ImportPlan { jobs }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str) -> ImportCandidate {
        ImportCandidate {
            local_path: PathBuf::from(format!("/tmp/{name}")),
            filename: name.to_string(),
            size: 42,
            suggested: None,
        }
    }

    #[test]
    fn scan_finds_rom_files_at_depth_one_and_two_but_not_three() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("top.gba"), b"a").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/nested.nes"), b"b").unwrap();
        fs::create_dir(dir.path().join("sub/deeper")).unwrap();
        fs::write(dir.path().join("sub/deeper/too_deep.sfc"), b"c").unwrap();
        fs::write(dir.path().join("notes.txt"), b"d").unwrap();

        let found = scan_local_dir(dir.path()).unwrap();
        let names: Vec<&str> = found.iter().map(|c| c.filename.as_str()).collect();
        assert!(names.contains(&"top.gba"));
        assert!(names.contains(&"nested.nes"));
        assert!(!names.contains(&"too_deep.sfc"));
        assert!(!names.contains(&"notes.txt"));
    }

    #[test]
    fn scan_sorts_newest_modified_first() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("older.gba");
        let newer = dir.path().join("newer.gba");
        fs::write(&older, b"a").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&newer, b"b").unwrap();

        let found = scan_local_dir(dir.path()).unwrap();
        assert_eq!(found[0].filename, "newer.gba");
        assert_eq!(found[1].filename, "older.gba");
    }

    #[test]
    fn scan_on_a_missing_directory_is_an_error_not_an_empty_list() {
        let missing = PathBuf::from("/definitely/does/not/exist/anywhere");
        assert!(scan_local_dir(&missing).is_err());
    }

    #[test]
    fn scan_populates_the_extension_based_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("game.gba"), b"a").unwrap();
        fs::write(dir.path().join("disc.chd"), b"b").unwrap();

        let found = scan_local_dir(dir.path()).unwrap();
        let gba = found.iter().find(|c| c.filename == "game.gba").unwrap();
        let chd = found.iter().find(|c| c.filename == "disc.chd").unwrap();
        assert_eq!(gba.suggested, Some("GBA"));
        assert_eq!(chd.suggested, None, "ambiguous extension must not guess a console");
    }

    #[test]
    fn parses_a_bare_unquoted_path() {
        let paths = parse_dropped_paths("/home/b/rom.gba");
        assert_eq!(paths, vec![PathBuf::from("/home/b/rom.gba")]);
    }

    #[test]
    fn parses_a_single_quoted_path_with_spaces() {
        let paths = parse_dropped_paths("'/home/b/Some ROM.gba'");
        assert_eq!(paths, vec![PathBuf::from("/home/b/Some ROM.gba")]);
    }

    #[test]
    fn parses_a_double_quoted_path_with_spaces() {
        let paths = parse_dropped_paths("\"/home/b/Some ROM.gba\"");
        assert_eq!(paths, vec![PathBuf::from("/home/b/Some ROM.gba")]);
    }

    #[test]
    fn parses_a_backslash_escaped_path() {
        let paths = parse_dropped_paths(r"/home/b/Some\ ROM.gba");
        assert_eq!(paths, vec![PathBuf::from("/home/b/Some ROM.gba")]);
    }

    #[test]
    fn parses_a_percent_encoded_file_uri() {
        let paths = parse_dropped_paths("file:///home/b/Some%20ROM.gba");
        assert_eq!(paths, vec![PathBuf::from("/home/b/Some ROM.gba")]);
    }

    #[test]
    fn parses_multiple_files_on_separate_lines() {
        let paths = parse_dropped_paths("/home/b/one.gba\n/home/b/two.nes\n");
        assert_eq!(paths, vec![PathBuf::from("/home/b/one.gba"), PathBuf::from("/home/b/two.nes")]);
    }

    #[test]
    fn parses_multiple_quoted_files_space_separated_on_one_line() {
        let paths = parse_dropped_paths("'/home/b/one.gba' '/home/b/two.nes'");
        assert_eq!(paths, vec![PathBuf::from("/home/b/one.gba"), PathBuf::from("/home/b/two.nes")]);
    }

    #[test]
    fn ignores_blank_lines() {
        let paths = parse_dropped_paths("\n/home/b/one.gba\n\n");
        assert_eq!(paths, vec![PathBuf::from("/home/b/one.gba")]);
    }

    #[test]
    fn candidate_from_path_rejects_directories_dotfiles_and_unknown_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("a_dir.gba");
        fs::create_dir(&subdir).unwrap();
        assert!(candidate_from_path(&subdir).is_none(), "a directory is not a ROM");

        let dotfile = dir.path().join(".hidden.gba");
        fs::write(&dotfile, b"a").unwrap();
        assert!(candidate_from_path(&dotfile).is_none());

        let unknown = dir.path().join("readme.txt");
        fs::write(&unknown, b"a").unwrap();
        assert!(candidate_from_path(&unknown).is_none());

        let missing = dir.path().join("ghost.gba");
        assert!(
            candidate_from_path(&missing).is_none(),
            "must not fabricate a candidate for a path that isn't there"
        );
    }

    #[test]
    fn plan_builds_the_device_remote_path() {
        let built = plan(&[(candidate("Pokemon Emerald.gba"), "GBA")]);
        assert_eq!(built.jobs.len(), 1);
        assert_eq!(built.jobs[0].remote_path, "/mnt/SDCARD/Roms/GBA/Pokemon Emerald.gba");
        assert_eq!(built.jobs[0].console_folder, "GBA");
        assert_eq!(built.jobs[0].size, 42);
    }

    #[test]
    fn plan_of_no_selections_is_empty() {
        assert!(plan(&[]).jobs.is_empty());
    }
}
