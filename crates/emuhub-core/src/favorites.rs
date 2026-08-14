//! Quirk 1 — `favourite.json` and `recentlist.json` are **NDJSON**: one JSON
//! object per line, no array wrapper, no commas. Writing a well-formed JSON
//! array will silently break the favourites list on the device.
//!
//! `read_*` functions are lenient (skip and log malformed lines, matching
//! the Swift original's `decode` + continue behaviour) since a single bad
//! entry — written by some other tool, or a half-flushed write — shouldn't
//! nuke the whole list. `write_*` is strict: any serialization failure is
//! propagated, because a partial write to the device is worse than no write.

use serde::{de::DeserializeOwned, Serialize};

use crate::models::{FavoriteGame, PlayHistoryEntry};

/// Parses NDJSON text into `T`, skipping (and reporting via `tracing::warn`)
/// any line that fails to decode.
pub fn parse_ndjson<T: DeserializeOwned>(text: &str) -> Vec<T> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| match serde_json::from_str::<T>(line) {
            Ok(value) => Some(value),
            Err(err) => {
                tracing::warn!(%err, line = %line.chars().take(100).collect::<String>(), "failed to parse NDJSON line");
                None
            }
        })
        .collect()
}

/// Serializes `items` as NDJSON (one compact JSON object per line, `\n`
/// separated, no trailing newline — matches the device's own files).
pub fn write_ndjson<T: Serialize>(items: &[T]) -> serde_json::Result<String> {
    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        lines.push(serde_json::to_string(item)?);
    }
    Ok(lines.join("\n"))
}

/// Parses `favourite.json` contents.
pub fn parse_favorites(text: &str) -> Vec<FavoriteGame> {
    parse_ndjson(text)
}

/// Serializes a favourites list back to `favourite.json` NDJSON format.
pub fn write_favorites(favorites: &[FavoriteGame]) -> serde_json::Result<String> {
    write_ndjson(favorites)
}

/// Parses `recentlist.json` / `recentlist-hidden.json` contents, filtering
/// out entries with no `rompath` (app launches, not games).
pub fn parse_recents(text: &str) -> Vec<PlayHistoryEntry> {
    parse_ndjson::<PlayHistoryEntry>(text).into_iter().filter(|entry| entry.rompath.is_some()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::GameFile;

    const FIXTURE_FAVORITES: &str = include_str!("../tests/fixtures/favourite.json");
    const FIXTURE_RECENTS: &str = include_str!("../tests/fixtures/recentlist.json");

    #[test]
    fn parses_captured_favourite_fixture() {
        let favs = parse_favorites(FIXTURE_FAVORITES);
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[0].label, "Pokemon - Emerald");
        assert_eq!(favs[0].kind, 5);
        assert_eq!(favs[1].label, "Wario Land 4");
    }

    #[test]
    fn round_trips_favourites_as_ndjson_not_an_array() {
        let favs = parse_favorites(FIXTURE_FAVORITES);
        let written = write_favorites(&favs).unwrap();

        // Must NOT be a JSON array — no leading '[', one object per line.
        assert!(!written.trim_start().starts_with('['));
        assert_eq!(written.lines().count(), favs.len());

        let reparsed = parse_favorites(&written);
        assert_eq!(favs, reparsed);
    }

    #[test]
    fn skips_malformed_lines_without_dropping_the_rest() {
        let text = "{\"label\":\"Good\",\"launch\":\"x\",\"type\":5,\"rompath\":\"/a\"}\nnot json at all\n{\"label\":\"Also Good\",\"launch\":\"x\",\"type\":5,\"rompath\":\"/b\"}";
        let favs = parse_favorites(text);
        assert_eq!(favs.len(), 2);
        assert_eq!(favs[0].label, "Good");
        assert_eq!(favs[1].label, "Also Good");
    }

    #[test]
    fn parses_recents_and_drops_app_launch_entries() {
        let recents = parse_recents(FIXTURE_RECENTS);
        // Fixture has 3 lines: 2 games + 1 app launch (no rompath).
        assert_eq!(recents.len(), 2);
        assert!(recents.iter().all(|e| e.rompath.is_some()));
    }

    #[test]
    fn builds_a_well_formed_favorite_from_a_game() {
        let game = GameFile {
            path: "/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba".to_string(),
            name: "Metroid Fusion.gba".to_string(),
            console_folder: "GBA".to_string(),
            extension: "gba".to_string(),
            size: None,
            image_path: "/mnt/SDCARD/Roms/GBA/Imgs/Metroid Fusion.png".to_string(),
        };
        let fav = FavoriteGame::for_game(&game);
        assert_eq!(fav.label, "Metroid Fusion");
        assert_eq!(fav.launch, "/mnt/SDCARD/Emu/GBA/launch.sh");
        assert_eq!(fav.kind, 5);
        assert_eq!(fav.rompath, game.path);
        assert_eq!(fav.imgpath.as_deref(), Some("/mnt/SDCARD/Roms/GBA/Imgs/Metroid Fusion.png"));
    }

    #[test]
    fn empty_file_parses_to_empty_list() {
        assert!(parse_favorites("").is_empty());
        assert!(parse_favorites("\n\n  \n").is_empty());
    }
}
