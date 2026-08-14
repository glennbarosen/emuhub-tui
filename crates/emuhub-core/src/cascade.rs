//! What deleting or renaming a ROM *actually* has to touch.
//!
//! A ROM on this device is not one file. Its box art lives in a sibling
//! `Imgs/` folder, its saves live under a RetroArch core directory that has
//! nothing to do with the console folder (Quirk 3), and it may be referenced
//! by name and path in both `favourite.json` and `recentlist.json`. The Swift
//! original moved or removed the ROM alone, leaving the rest orphaned — dead
//! favourites, art for a game that no longer exists, saves that nothing can
//! reach.
//!
//! Everything here is a pure plan: given a game and the current state of the
//! card, work out the exact set of paths and file rewrites, with no I/O. That
//! makes the cascade unit-testable, and — because a plan can be rendered —
//! lets the UI show precisely what is about to happen *before* it happens —
//! a `--dry-run` guarantee in a different shape. See `docs/DEVICE-PROTOCOL.md`
//! §6 for the full set of files a single ROM drags along with it.

use crate::models::{FavoriteGame, GameFile, PlayHistoryEntry, SaveState};
use crate::saves;

/// Everything a delete has to remove or rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletePlan {
    /// Files to remove, ROM first. Includes the box art and every save,
    /// thumbnails included.
    pub removals: Vec<String>,
    /// The favourites list without this game, if it was in it.
    pub favorites: Option<Vec<FavoriteGame>>,
    /// The recent list without this game, if it was in it.
    pub recents: Option<Vec<PlayHistoryEntry>>,
    /// Onion's per-console ROM cache, which still lists the deleted ROM.
    pub console_cache_db: String,
}

/// Everything a rename has to move or rewrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenamePlan {
    /// `(from, to)` pairs, ROM first.
    pub renames: Vec<(String, String)>,
    pub favorites: Option<Vec<FavoriteGame>>,
    pub recents: Option<Vec<PlayHistoryEntry>>,
    pub console_cache_db: String,
    /// The game as it will be once the rename lands, so the UI can update in
    /// place instead of forcing a full rescan.
    pub new_game: GameFile,
}

/// Rejected new names: anything that would escape the console folder or
/// produce a file the scanner can't see again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RenameError {
    #[error("name cannot be empty")]
    Empty,
    #[error("name cannot contain '/'")]
    ContainsSlash,
    #[error("name cannot be '.' or '..'")]
    DotSegment,
}

impl DeletePlan {
    /// One line per thing that will change, for the confirmation dialog.
    /// Rewrites are described rather than listed as paths, since "removed from
    /// favourites" is what the user cares about, not the file it lives in.
    pub fn summary(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.removals.iter().map(|p| format!("delete  {p}")).collect();
        if self.favorites.is_some() {
            lines.push("update  favourite.json (remove entry)".to_string());
        }
        if self.recents.is_some() {
            lines.push("update  recentlist.json (remove entry)".to_string());
        }
        lines.push(format!("reset   {} (Onion rebuilds it)", self.console_cache_db));
        lines
    }
}

impl RenamePlan {
    pub fn summary(&self) -> Vec<String> {
        let mut lines: Vec<String> =
            self.renames.iter().map(|(from, to)| format!("rename  {from}\n     -> {to}")).collect();
        if self.favorites.is_some() {
            lines.push("update  favourite.json (retarget entry)".to_string());
        }
        if self.recents.is_some() {
            lines.push("update  recentlist.json (retarget entry)".to_string());
        }
        lines.push(format!("reset   {} (Onion rebuilds it)", self.console_cache_db));
        lines
    }
}

/// Onion keeps a per-console ROM cache next to the ROMs. Left untouched after
/// a delete or rename it keeps listing the old entry, so the handheld shows a
/// game that no longer exists until something forces a rescan.
pub fn console_cache_db(console_folder: &str) -> String {
    format!("/mnt/SDCARD/Roms/{console_folder}/{console_folder}_cache6.db")
}

/// Plans the full cascade for deleting `game`.
pub fn delete_plan(
    game: &GameFile,
    all_states: &[SaveState],
    favorites: &[FavoriteGame],
    recents: &[PlayHistoryEntry],
) -> DeletePlan {
    let mut removals = vec![game.path.clone(), game.image_path.clone()];
    for state in saves::states_for_game(all_states, game) {
        removals.push(state.path);
        if let Some(thumbnail) = state.thumbnail_path {
            removals.push(thumbnail);
        }
    }

    DeletePlan {
        removals,
        favorites: favorites_without(favorites, &game.path),
        recents: recents_without(recents, &game.path),
        console_cache_db: console_cache_db(&game.console_folder),
    }
}

/// Plans the full cascade for renaming `game` to `new_stem` (the name without
/// its ROM extension — the extension is preserved, since changing it would
/// change which emulator the file belongs to).
pub fn rename_plan(
    game: &GameFile,
    new_stem: &str,
    all_states: &[SaveState],
    favorites: &[FavoriteGame],
    recents: &[PlayHistoryEntry],
) -> Result<RenamePlan, RenameError> {
    let new_stem = new_stem.trim();
    if new_stem.is_empty() {
        return Err(RenameError::Empty);
    }
    if new_stem.contains('/') {
        return Err(RenameError::ContainsSlash);
    }
    if new_stem == "." || new_stem == ".." {
        return Err(RenameError::DotSegment);
    }

    let dir = parent_of(&game.path);
    let new_filename = format!("{new_stem}.{}", game.extension);
    let new_path = format!("{dir}/{new_filename}");
    let new_image_path = format!("{dir}/Imgs/{new_stem}.png");

    let mut renames =
        vec![(game.path.clone(), new_path.clone()), (game.image_path.clone(), new_image_path.clone())];

    // Saves are named after the ROM, so they move with it — otherwise the
    // renamed game loads with no save history at all. Which prefix to swap
    // depends on how the state was named in the first place: some cores keep
    // the ROM extension in the state filename, some drop it.
    for state in saves::states_for_game(all_states, game) {
        let new_prefix = if state.game_name == game.name { &new_filename } else { new_stem };
        let Some(suffix) = state.name.strip_prefix(&state.game_name) else {
            continue;
        };
        let renamed = format!("{}/{new_prefix}{suffix}", parent_of(&state.path));
        if let Some(thumbnail) = &state.thumbnail_path {
            renames.push((thumbnail.clone(), format!("{renamed}.png")));
        }
        renames.push((state.path.clone(), renamed));
    }

    let new_game = GameFile {
        path: new_path,
        name: new_filename,
        console_folder: game.console_folder.clone(),
        extension: game.extension.clone(),
        size: game.size,
        image_path: new_image_path,
    };

    Ok(RenamePlan {
        favorites: favorites_retargeted(favorites, &game.path, &new_game),
        recents: recents_retargeted(recents, &game.path, &new_game),
        console_cache_db: console_cache_db(&game.console_folder),
        renames,
        new_game,
    })
}

/// The favourites list with `path` dropped, or `None` if it wasn't there.
fn favorites_without(favorites: &[FavoriteGame], path: &str) -> Option<Vec<FavoriteGame>> {
    let remaining: Vec<FavoriteGame> =
        favorites.iter().filter(|f| f.normalized_path() != path).cloned().collect();
    (remaining.len() != favorites.len()).then_some(remaining)
}

fn recents_without(recents: &[PlayHistoryEntry], path: &str) -> Option<Vec<PlayHistoryEntry>> {
    let remaining: Vec<PlayHistoryEntry> =
        recents.iter().filter(|e| e.normalized_path().as_deref() != Some(path)).cloned().collect();
    (remaining.len() != recents.len()).then_some(remaining)
}

/// The favourites list with the entry for `old_path` pointed at `new_game`,
/// or `None` if the game wasn't favourited.
///
/// Rebuilt through `FavoriteGame::for_game` rather than patched field by
/// field, so the entry stays exactly the shape the device expects.
fn favorites_retargeted(
    favorites: &[FavoriteGame],
    old_path: &str,
    new_game: &GameFile,
) -> Option<Vec<FavoriteGame>> {
    if !favorites.iter().any(|f| f.normalized_path() == old_path) {
        return None;
    }
    Some(
        favorites
            .iter()
            .map(
                |f| {
                    if f.normalized_path() == old_path {
                        FavoriteGame::for_game(new_game)
                    } else {
                        f.clone()
                    }
                },
            )
            .collect(),
    )
}

fn recents_retargeted(
    recents: &[PlayHistoryEntry],
    old_path: &str,
    new_game: &GameFile,
) -> Option<Vec<PlayHistoryEntry>> {
    if !recents.iter().any(|e| e.normalized_path().as_deref() == Some(old_path)) {
        return None;
    }
    Some(
        recents
            .iter()
            .map(|entry| {
                if entry.normalized_path().as_deref() != Some(old_path) {
                    return entry.clone();
                }
                PlayHistoryEntry {
                    label: new_game.display_name().to_string(),
                    rompath: Some(new_game.path.clone()),
                    imgpath: Some(new_game.image_path.clone()),
                    ..entry.clone()
                }
            })
            .collect(),
    )
}

fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAVES_FIXTURE: &str = include_str!("../tests/fixtures/find_saves.txt");

    fn emerald() -> GameFile {
        GameFile {
            path: "/mnt/SDCARD/Roms/GBA/Pokemon - Emerald.gba".to_string(),
            name: "Pokemon - Emerald.gba".to_string(),
            console_folder: "GBA".to_string(),
            extension: "gba".to_string(),
            size: None,
            image_path: "/mnt/SDCARD/Roms/GBA/Imgs/Pokemon - Emerald.png".to_string(),
        }
    }

    fn states() -> Vec<SaveState> {
        saves::parse_save_listing(SAVES_FIXTURE)
    }

    fn favorited(game: &GameFile) -> Vec<FavoriteGame> {
        vec![FavoriteGame::for_game(game)]
    }

    fn recent_for(game: &GameFile) -> Vec<PlayHistoryEntry> {
        vec![PlayHistoryEntry {
            label: game.display_name().to_string(),
            launch: format!("/mnt/SDCARD/Emu/{}/launch.sh", game.console_folder),
            kind: 5,
            imgpath: Some(game.image_path.clone()),
            // Deliberately the un-normalized form the device actually writes.
            rompath: Some(format!("/mnt/SDCARD/Roms/GBA/../../Roms/GBA/{}", game.name)),
        }]
    }

    #[test]
    fn delete_takes_the_rom_its_art_and_every_save_including_thumbnails() {
        let game = emerald();
        let plan = delete_plan(&game, &states(), &[], &[]);

        assert_eq!(plan.removals[0], game.path, "the ROM itself comes first");
        assert!(plan.removals.contains(&game.image_path));
        // Fixture: .state, .state1, .state21, .srm — plus .png thumbnails for
        // slots 1 and 21.
        assert!(plan
            .removals
            .contains(&"/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon - Emerald.state21".to_string()));
        assert!(plan.removals.contains(
            &"/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon - Emerald.state21.png".to_string()
        ));
        assert!(plan
            .removals
            .contains(&"/mnt/SDCARD/Saves/CurrentProfile/saves/gpsp/Pokemon - Emerald.srm".to_string()));
        assert_eq!(plan.removals.len(), 8);
    }

    #[test]
    fn delete_leaves_another_games_saves_alone() {
        let plan = delete_plan(&emerald(), &states(), &[], &[]);
        assert!(
            !plan.removals.iter().any(|p| p.contains("Wario") || p.contains("Kirby") || p.contains("Sonic")),
            "cascade must not reach past the game being deleted"
        );
    }

    #[test]
    fn delete_rewrites_favourites_and_recents_only_when_the_game_is_listed() {
        let game = emerald();

        let untouched = delete_plan(&game, &states(), &[], &[]);
        assert!(untouched.favorites.is_none());
        assert!(untouched.recents.is_none());

        let listed = delete_plan(&game, &states(), &favorited(&game), &recent_for(&game));
        assert_eq!(listed.favorites.unwrap(), Vec::new(), "the only favourite was this game");
        assert_eq!(listed.recents.unwrap(), Vec::new(), "a `../../` rompath must still match");
    }

    #[test]
    fn delete_resets_the_console_rom_cache_so_onion_drops_the_ghost_entry() {
        let plan = delete_plan(&emerald(), &[], &[], &[]);
        assert_eq!(plan.console_cache_db, "/mnt/SDCARD/Roms/GBA/GBA_cache6.db");
    }

    #[test]
    fn rename_moves_the_rom_its_art_and_its_saves_together() {
        let game = emerald();
        let plan = rename_plan(&game, "Pokemon Emerald", &states(), &[], &[]).unwrap();

        assert_eq!(
            plan.renames[0],
            (game.path.clone(), "/mnt/SDCARD/Roms/GBA/Pokemon Emerald.gba".to_string())
        );
        assert_eq!(
            plan.renames[1],
            (game.image_path.clone(), "/mnt/SDCARD/Roms/GBA/Imgs/Pokemon Emerald.png".to_string())
        );
        assert!(plan.renames.contains(&(
            "/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon - Emerald.state21".to_string(),
            "/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon Emerald.state21".to_string(),
        )));
        assert!(plan.renames.contains(&(
            "/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon - Emerald.state21.png".to_string(),
            "/mnt/SDCARD/Saves/CurrentProfile/states/gpsp/Pokemon Emerald.state21.png".to_string(),
        )));
    }

    #[test]
    fn rename_preserves_a_state_that_kept_the_rom_extension() {
        let sonic = GameFile {
            path: "/mnt/SDCARD/Roms/MD/Sonic.md".to_string(),
            name: "Sonic.md".to_string(),
            console_folder: "MD".to_string(),
            extension: "md".to_string(),
            size: None,
            image_path: "/mnt/SDCARD/Roms/MD/Imgs/Sonic.png".to_string(),
        };
        let plan = rename_plan(&sonic, "Sonic 1", &states(), &[], &[]).unwrap();

        assert!(
            plan.renames.contains(&(
                "/mnt/SDCARD/Saves/CurrentProfile/states/picodrive/Sonic.md.state".to_string(),
                "/mnt/SDCARD/Saves/CurrentProfile/states/picodrive/Sonic 1.md.state".to_string(),
            )),
            "a `Game.md.state` must keep its extension segment, got {:?}",
            plan.renames
        );
    }

    #[test]
    fn rename_keeps_the_rom_extension_and_reports_the_resulting_game() {
        let plan = rename_plan(&emerald(), "Emerald", &[], &[], &[]).unwrap();
        assert_eq!(plan.new_game.name, "Emerald.gba");
        assert_eq!(plan.new_game.extension, "gba");
        assert_eq!(plan.new_game.console_folder, "GBA");
        assert_eq!(plan.new_game.path, "/mnt/SDCARD/Roms/GBA/Emerald.gba");
        assert_eq!(plan.new_game.image_path, "/mnt/SDCARD/Roms/GBA/Imgs/Emerald.png");
    }

    #[test]
    fn rename_retargets_the_favourite_entry_to_the_new_path() {
        let game = emerald();
        let plan = rename_plan(&game, "Emerald", &[], &favorited(&game), &[]).unwrap();

        let favorites = plan.favorites.expect("a favourited game's entry must be retargeted");
        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].rompath, "/mnt/SDCARD/Roms/GBA/Emerald.gba");
        assert_eq!(favorites[0].label, "Emerald");
        assert_eq!(favorites[0].kind, 5, "the entry must stay the shape the device expects");
    }

    #[test]
    fn rename_retargets_the_recent_entry_and_leaves_others_untouched() {
        let game = emerald();
        let other = PlayHistoryEntry {
            label: "Wario Land 4".to_string(),
            launch: "/mnt/SDCARD/Emu/GBA/launch.sh".to_string(),
            kind: 5,
            imgpath: None,
            rompath: Some("/mnt/SDCARD/Roms/GBA/Wario Land 4.gba".to_string()),
        };
        let mut recents = recent_for(&game);
        recents.push(other.clone());

        let plan = rename_plan(&game, "Emerald", &[], &[], &recents).unwrap();
        let updated = plan.recents.unwrap();

        assert_eq!(updated[0].rompath.as_deref(), Some("/mnt/SDCARD/Roms/GBA/Emerald.gba"));
        assert_eq!(updated[0].label, "Emerald");
        assert_eq!(updated[1], other, "an unrelated recent entry must survive byte-identical");
    }

    #[test]
    fn rename_rejects_names_that_would_escape_the_console_folder() {
        let game = emerald();
        assert_eq!(rename_plan(&game, "", &[], &[], &[]), Err(RenameError::Empty));
        assert_eq!(rename_plan(&game, "   ", &[], &[], &[]), Err(RenameError::Empty));
        assert_eq!(rename_plan(&game, "../evil", &[], &[], &[]), Err(RenameError::ContainsSlash));
        assert_eq!(rename_plan(&game, "..", &[], &[], &[]), Err(RenameError::DotSegment));
    }

    #[test]
    fn summaries_describe_every_effect_of_the_plan() {
        let game = emerald();
        let plan = delete_plan(&game, &states(), &favorited(&game), &recent_for(&game));
        let summary = plan.summary();

        assert!(summary.iter().any(|l| l.contains("favourite.json")));
        assert!(summary.iter().any(|l| l.contains("recentlist.json")));
        assert!(summary.iter().any(|l| l.contains("GBA_cache6.db")));
        assert_eq!(summary.len(), plan.removals.len() + 3);
    }
}
