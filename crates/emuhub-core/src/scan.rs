//! Parses the output of a single exec-channel command —
//! `find /mnt/SDCARD/Roms -maxdepth 2 -type f` — into a library of
//! `GameFile`s grouped by console.
//!
//! This replaces the Swift original's `ConnectionManager.preloadData()` bug:
//! that function walked all 25 known consoles one SFTP
//! `listDirectory` call at a time, including the ~20 with no ROMs on a
//! typical card. One `find` + one parse gets the same result in one round
//! trip.
//!
//! `-maxdepth 2` naturally excludes `Imgs/*.png` (those live at depth 3) and
//! `favourite.json`/`recentlist*.json` are excluded by the ROM-extension
//! filter, not by depth (they sit at depth 1, same as a console folder would
//! be depth 1 — the file itself is what's filtered).

use std::collections::BTreeMap;

use crate::consoles;
use crate::models::GameFile;

const ROMS_ROOT: &str = "/mnt/SDCARD/Roms";

/// Parses raw `find` stdout into `GameFile`s. Unrecognized extensions,
/// dotfiles, and paths that aren't directly under `Roms/{CONSOLE}/` are
/// silently skipped.
///
/// Accepts both line shapes the transport can produce, so one parser serves
/// the size-carrying command *and* the plain-`find` fallback for cards whose
/// busybox lacks `stat -c`:
///
/// ```text
/// /mnt/SDCARD/Roms/GBA/Metroid Fusion.gba              (bare path)
/// 8388608 /mnt/SDCARD/Roms/GBA/Metroid Fusion.gba      (stat -c '%s %n')
/// ```
///
/// Only a leading run of digits followed by a space counts as a size — ROM
/// filenames routinely contain spaces, so the split is on the *first* space
/// and only when everything before it is numeric.
pub fn parse_find_output(output: &str) -> Vec<GameFile> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|line| {
            let (size, path) = split_size_prefix(line);
            let mut game = parse_rom_path(path)?;
            game.size = size;
            Some(game)
        })
        .collect()
}

/// Splits a `stat -c '%s %n'` line into its size and path, or returns the
/// whole line as a path when there's no numeric prefix.
fn split_size_prefix(line: &str) -> (Option<u64>, &str) {
    let Some((head, rest)) = line.split_once(' ') else {
        return (None, line);
    };
    match head.parse::<u64>() {
        Ok(size) if !head.is_empty() => (Some(size), rest.trim_start()),
        _ => (None, line),
    }
}

/// Builds a `GameFile` from one absolute device path, or `None` if the path
/// isn't a ROM sitting directly inside a console folder.
///
/// Public because it's also how a favourite/recent entry's `rompath` is turned
/// back into a game when the current scan didn't see it (deleted ROM, swapped
/// card) — resolving those through the same parser keeps a synthesized entry
/// byte-identical to a scanned one.
pub fn parse_rom_path(path: &str) -> Option<GameFile> {
    let rel = path.strip_prefix(ROMS_ROOT)?.trim_start_matches('/');
    // A bare filename (no '/') means it sits directly under Roms/ — not a ROM.
    let (console_folder, filename) = rel.split_once('/')?;

    // Reject anything with further path segments (e.g. would only happen if
    // maxdepth were higher than expected) — a ROM must be directly inside
    // its console folder.
    if filename.contains('/') {
        return None;
    }
    if filename.starts_with('.') {
        return None;
    }

    let extension = filename.rsplit_once('.').map(|(_, ext)| ext)?.to_string();
    if !consoles::is_rom_extension(&extension) {
        return None;
    }

    let basename = filename.strip_suffix(&format!(".{extension}")).unwrap_or(filename);
    let image_path = format!("{ROMS_ROOT}/{console_folder}/Imgs/{basename}.png");

    Some(GameFile {
        path: path.to_string(),
        name: filename.to_string(),
        console_folder: console_folder.to_string(),
        extension,
        size: None,
        image_path,
    })
}

/// Groups a flat game list by console folder, preserving `consoles::ALL_SYSTEMS`
/// order and including consoles with zero ROMs (so the sidebar can still list
/// them with a "0" count).
pub fn group_by_console(games: Vec<GameFile>) -> Vec<(&'static str, Vec<GameFile>)> {
    let mut by_folder: BTreeMap<&str, Vec<GameFile>> = BTreeMap::new();
    for game in games {
        // Leak-free: look up the static folder string so keys are 'static.
        if let Some(console) = consoles::by_folder(&game.console_folder) {
            by_folder.entry(console.folder).or_default().push(game);
        }
    }

    consoles::ALL_SYSTEMS.iter().map(|c| (c.folder, by_folder.remove(c.folder).unwrap_or_default())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/find_roms.txt");

    #[test]
    fn parses_captured_find_fixture() {
        let games = parse_find_output(FIXTURE);
        // 3 GBA + 1 GB + 2 FC + 1 PS = 7 recognized ROMs.
        // (favourite.json, recentlist-hidden.json, GBA_cache6.db excluded.)
        assert_eq!(games.len(), 7);
    }

    #[test]
    fn excludes_non_rom_files_at_roms_root() {
        let games = parse_find_output(FIXTURE);
        assert!(!games.iter().any(|g| g.name.ends_with(".json")));
        assert!(!games.iter().any(|g| g.name.ends_with(".db")));
    }

    #[test]
    fn builds_correct_image_path() {
        let games = parse_find_output(FIXTURE);
        let emerald = games.iter().find(|g| g.name == "Pokemon - Emerald.gba").unwrap();
        assert_eq!(emerald.image_path, "/mnt/SDCARD/Roms/GBA/Imgs/Pokemon - Emerald.png");
        assert_eq!(emerald.console_folder, "GBA");
        assert_eq!(emerald.extension, "gba");
    }

    #[test]
    fn groups_by_console_including_zero_count_consoles() {
        let games = parse_find_output(FIXTURE);
        let grouped = group_by_console(games);

        let gba = grouped.iter().find(|(f, _)| *f == "GBA").unwrap();
        assert_eq!(gba.1.len(), 3);

        let nds = grouped.iter().find(|(f, _)| *f == "NDS").unwrap();
        assert_eq!(nds.1.len(), 0);

        // All 25 known consoles present even with no ROMs.
        assert_eq!(grouped.len(), consoles::ALL_SYSTEMS.len());
    }

    #[test]
    fn handles_case_insensitive_extensions() {
        let games = parse_find_output("/mnt/SDCARD/Roms/GBA/Foo.GBA\n");
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].extension, "GBA");
    }

    #[test]
    fn empty_input_yields_empty_library() {
        assert!(parse_find_output("").is_empty());
    }

    #[test]
    fn bare_find_output_leaves_size_unknown() {
        let games = parse_find_output(FIXTURE);
        assert!(games.iter().all(|g| g.size.is_none()));
    }

    #[test]
    fn parses_stat_prefixed_output() {
        let games = parse_find_output(
            "8388608 /mnt/SDCARD/Roms/GBA/Metroid Fusion.gba\n\
             1048576 /mnt/SDCARD/Roms/GB/Tetris.gb\n",
        );
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].name, "Metroid Fusion.gba");
        assert_eq!(games[0].size, Some(8_388_608));
        assert_eq!(games[1].size, Some(1_048_576));
    }

    #[test]
    fn size_prefix_split_survives_spaces_in_filenames() {
        // The split is on the *first* space; everything after it, spaces
        // included, is the path.
        let games = parse_find_output("42 /mnt/SDCARD/Roms/GBA/Pokemon - Emerald.gba\n");
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Pokemon - Emerald.gba");
        assert_eq!(games[0].size, Some(42));
    }

    #[test]
    fn a_non_numeric_first_word_is_part_of_the_path_not_a_size() {
        // Nothing on the device produces this, but a path can't be allowed to
        // lose its first word to a failed size parse.
        let games = parse_find_output("/mnt/SDCARD/Roms/GBA/Final Fantasy.gba\n");
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].name, "Final Fantasy.gba");
        assert_eq!(games[0].size, None);
    }

    #[test]
    fn mixed_output_parses_both_shapes() {
        let games = parse_find_output(
            "8388608 /mnt/SDCARD/Roms/GBA/Metroid Fusion.gba\n\
             /mnt/SDCARD/Roms/GB/Tetris.gb\n",
        );
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].size, Some(8_388_608));
        assert_eq!(games[1].size, None);
    }
}
