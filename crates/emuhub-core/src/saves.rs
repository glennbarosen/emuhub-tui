//! Quirk 3 — save states and save files are filed by **RetroArch core**, not
//! by console folder: a Game Boy Advance state lives in `states/gpsp/`, never
//! `states/GBA/`. The console→core table (`consoles::CONSOLE_CORE_NAMES`) is
//! the mapping that makes them findable at all.
//!
//! Everything here is pure: it turns one `find` listing over the save tree
//! into `SaveState`s, and answers "which of these belong to this ROM?". The
//! single round trip that produces the listing lives in `transport`.
//!
//! Naming, per `docs/DEVICE-PROTOCOL.md` §5:
//!
//! ```text
//! Game.state        → slot 0   (plain state)
//! Game.state.auto   → slot 0   (auto-save)
//! Game.state21      → slot 21
//! Game.state21.png  → the thumbnail for slot 21, not a state of its own
//! Game.srm/.sav     → slot -1  (a save *file*, not a state)
//! ```

use std::collections::HashMap;

use crate::consoles;
use crate::models::{GameFile, SaveState};

pub const STATES_ROOT: &str = "/mnt/SDCARD/Saves/CurrentProfile/states";
pub const SAVES_ROOT: &str = "/mnt/SDCARD/Saves/CurrentProfile/saves";

/// Slot marker for a save file (`.srm`/`.sav`) rather than a save state,
/// matching `SaveState::display_name`'s contract.
const SLOT_SAVE_FILE: i32 = -1;

/// Save-file extensions — everything else that isn't a `.state*` is litter.
const SAVE_FILE_EXTENSIONS: &[&str] = &["srm", "sav"];

/// Parses a `find` listing of the save tree (one absolute path per line) into
/// save states and save files, pairing each with its `.png` thumbnail when the
/// listing contains one.
pub fn parse_save_listing(output: &str) -> Vec<SaveState> {
    let paths: Vec<&str> = output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();

    // Thumbnails are `{statefile}.png`, so the key is the sibling state's own
    // full path. Collected first because a thumbnail can appear before or
    // after its state in `find` output. Case-insensitive, matching
    // `consoles::is_rom_extension` — RetroArch's own extension casing isn't
    // something to bet a thumbnail pairing on.
    let thumbnails: HashMap<&str, &str> =
        paths.iter().filter_map(|path| strip_png_suffix(path).map(|state| (state, *path))).collect();

    paths
        .iter()
        .filter(|path| strip_png_suffix(path).is_none())
        .filter_map(|path| {
            let (game_name, slot_number) = parse_slot(filename_of(path)?)?;
            Some(SaveState {
                name: filename_of(path)?.to_string(),
                path: (*path).to_string(),
                game_name,
                slot_number,
                // `find` alone doesn't report sizes and busybox's `-printf`
                // support is not something to bet the listing on.
                size: None,
                thumbnail_path: thumbnails.get(path).map(|t| (*t).to_string()),
            })
        })
        .collect()
}

/// Case-insensitive `.png` suffix strip. Filenames are also matched via
/// `strip_prefix`/exact string compare elsewhere in this module, but the
/// extension itself is the one place a differently-cased thumbnail (`.PNG`)
/// would otherwise silently fail to pair with its save state.
fn strip_png_suffix(path: &str) -> Option<&str> {
    let cut = path.len().checked_sub(".png".len())?;
    path[cut..].eq_ignore_ascii_case(".png").then(|| &path[..cut])
}

/// The save states and save files belonging to `game`, ordered by how likely
/// the core directory is to be the right one (the console-named directory
/// first, then `consoles::core_names_for` order), then by slot.
///
/// The ordering matters because two consoles can share a core — `picodrive`
/// serves both MD and MS — so a basename can legitimately appear under
/// several directories and the caller wants the most plausible one first.
pub fn states_for_game(all: &[SaveState], game: &GameFile) -> Vec<SaveState> {
    let mut matched: Vec<SaveState> = all.iter().filter(|state| belongs_to(state, game)).cloned().collect();

    matched.sort_by_key(|state| (core_rank(state, game), state.slot_number));
    matched
}

/// True if `state`'s basename identifies `game`.
///
/// Checked against both the ROM's display name and its full filename, because
/// RetroArch keeps the ROM extension in the state name for some cores
/// (`Game.gba.state`) and drops it for others (`Game.state`).
///
/// Public so callers that only need a count (a "has saves" marker) can avoid
/// building and sorting the full per-game list.
pub fn belongs_to(state: &SaveState, game: &GameFile) -> bool {
    let stem = state.game_name.to_lowercase();
    stem == game.display_name().to_lowercase() || stem == game.name.to_lowercase()
}

/// Sort key: 0 for the console-folder-named directory, 1.. for each candidate
/// core in table order, and a large value for anything unrecognized (kept, but
/// last — an unknown core directory is still better than hiding the state).
fn core_rank(state: &SaveState, game: &GameFile) -> usize {
    let Some(dir) = core_dir_of(&state.path) else {
        return usize::MAX;
    };
    if dir.eq_ignore_ascii_case(&game.console_folder) {
        return 0;
    }
    consoles::core_names_for(&game.console_folder)
        .iter()
        .position(|core| core.eq_ignore_ascii_case(dir))
        .map(|i| i + 1)
        .unwrap_or(usize::MAX)
}

/// The core directory a save sits in — the last path segment before the file.
fn core_dir_of(path: &str) -> Option<&str> {
    let (dir, _) = path.rsplit_once('/')?;
    dir.rsplit_once('/').map(|(_, name)| name)
}

fn filename_of(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(_, name)| name).filter(|name| !name.is_empty())
}

/// Splits a save filename into its game basename and slot number, or `None` if
/// it isn't a save at all (RetroArch config litter — see
/// `consoles::EXCLUDED_SAVE_EXTENSIONS`).
fn parse_slot(filename: &str) -> Option<(String, i32)> {
    if let Some(stem) = filename.strip_suffix(".state.auto") {
        return Some((stem.to_string(), 0));
    }
    if let Some(stem) = filename.strip_suffix(".state") {
        return Some((stem.to_string(), 0));
    }

    // `.state{N}` — split at the last `.state` and require digits after it, so
    // a game whose own name contains "state" can't be mistaken for a slot.
    if let Some((stem, suffix)) = filename.rsplit_once(".state") {
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            return Some((stem.to_string(), suffix.parse().ok()?));
        }
    }

    let (stem, extension) = filename.rsplit_once('.')?;
    let extension = extension.to_lowercase();
    if SAVE_FILE_EXTENSIONS.contains(&extension.as_str()) {
        return Some((stem.to_string(), SLOT_SAVE_FILE));
    }
    if consoles::EXCLUDED_SAVE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/find_saves.txt");

    fn game(console: &str, name: &str, ext: &str) -> GameFile {
        GameFile {
            path: format!("/mnt/SDCARD/Roms/{console}/{name}.{ext}"),
            name: format!("{name}.{ext}"),
            console_folder: console.to_string(),
            extension: ext.to_string(),
            size: None,
            image_path: format!("/mnt/SDCARD/Roms/{console}/Imgs/{name}.png"),
        }
    }

    #[test]
    fn parses_every_slot_form() {
        assert_eq!(parse_slot("Game.state"), Some(("Game".to_string(), 0)));
        assert_eq!(parse_slot("Game.state.auto"), Some(("Game".to_string(), 0)));
        assert_eq!(parse_slot("Game.state1"), Some(("Game".to_string(), 1)));
        assert_eq!(parse_slot("Game.state21"), Some(("Game".to_string(), 21)));
        assert_eq!(parse_slot("Game.srm"), Some(("Game".to_string(), -1)));
        assert_eq!(parse_slot("Game.sav"), Some(("Game".to_string(), -1)));
    }

    #[test]
    fn rejects_config_litter() {
        for name in ["retroarch.cfg", "Game.state21.opt", "notes.txt", "core.log"] {
            assert_eq!(parse_slot(name), None, "{name} should not parse as a save");
        }
    }

    #[test]
    fn thumbnails_attach_to_their_state_and_are_not_states_themselves() {
        let states = parse_save_listing(FIXTURE);

        let slot21 = states.iter().find(|s| s.slot_number == 21).unwrap();
        assert_eq!(
            slot21.thumbnail_path.as_deref(),
            Some("/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon - Emerald.state21.png")
        );
        assert!(
            !states.iter().any(|s| s.path.ends_with(".png")),
            "a thumbnail must never be listed as a save state in its own right"
        );
    }

    #[test]
    fn thumbnail_pairing_is_case_insensitive_on_the_extension() {
        let listing = "/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Game.state\n\
             /mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Game.state.PNG";
        let states = parse_save_listing(listing);

        assert_eq!(states.len(), 1, "an upper-cased .PNG thumbnail must not be listed as its own state");
        assert_eq!(
            states[0].thumbnail_path.as_deref(),
            Some("/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Game.state.PNG")
        );
    }

    #[test]
    fn a_state_without_a_thumbnail_has_none() {
        let states = parse_save_listing(FIXTURE);
        let auto = states.iter().find(|s| s.name == "Wario Land 4.state.auto").unwrap();
        assert!(auto.thumbnail_path.is_none());
    }

    #[test]
    fn matches_a_game_by_display_name_and_orders_by_core_then_slot() {
        let states = parse_save_listing(FIXTURE);
        let emerald = game("GBA", "Pokemon - Emerald", "gba");

        let mine = states_for_game(&states, &emerald);
        // .srm (-1), .state (0), .state1, .state21 — all under gpsp, the
        // first core listed for GBA.
        assert_eq!(mine.iter().map(|s| s.slot_number).collect::<Vec<_>>(), vec![-1, 0, 1, 21]);
        assert!(mine.iter().all(|s| s.path.contains("/gpsp/")));
    }

    #[test]
    fn matches_a_state_that_kept_the_rom_extension() {
        let states = parse_save_listing(FIXTURE);
        let sonic = game("MD", "Sonic", "md");

        let mine = states_for_game(&states, &sonic);
        assert_eq!(mine.len(), 1, "Sonic.md.state must match the ROM Sonic.md");
        assert_eq!(mine[0].name, "Sonic.md.state");
    }

    #[test]
    fn preferred_core_sorts_ahead_of_a_later_one_for_the_same_basename() {
        let states = parse_save_listing(FIXTURE);
        let land = game("GB", "Kirby", "gb");

        let mine = states_for_game(&states, &land);
        // GB's table order is gambatte, gearboy, tgbdual — the gambatte copy
        // must come first even though `find` listed gearboy's first.
        assert_eq!(mine.len(), 2);
        assert!(mine[0].path.contains("/gambatte/"), "got {}", mine[0].path);
        assert!(mine[1].path.contains("/gearboy/"), "got {}", mine[1].path);
    }

    #[test]
    fn a_game_with_no_saves_gets_an_empty_list() {
        let states = parse_save_listing(FIXTURE);
        assert!(states_for_game(&states, &game("GBA", "Never Played", "gba")).is_empty());
    }

    #[test]
    fn empty_listing_is_not_an_error() {
        assert!(parse_save_listing("").is_empty());
        assert!(parse_save_listing("\n  \n").is_empty());
    }
}
