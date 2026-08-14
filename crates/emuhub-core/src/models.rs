//! Plain data models, ported from the SwiftUI original's `Models/*.swift`.
//!
//! These are intentionally dumb structs — no networking, no view logic. See
//! `path::normalize` for the one piece of load-bearing logic that lives
//! alongside a model (favourite/recent ROM path resolution).

use serde::{Deserialize, Serialize};

use crate::path::normalize_path;

/// A console/system entry (folder name + display metadata). The full static
/// list lives in `consoles::ALL_SYSTEMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Console {
    pub name: &'static str,
    pub folder: &'static str,
    pub icon: &'static str,
}

/// A ROM file discovered on the device (or in the offline cache).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameFile {
    /// Absolute remote path — doubles as a stable identity key.
    pub path: String,
    pub name: String,
    pub console_folder: String,
    pub extension: String,
    /// Byte size, when known (a bare `find` listing doesn't give us this for
    /// free; populated lazily via `stat` or left `None`).
    pub size: Option<u64>,
    /// `{console-dir}/Imgs/{basename}.png` — may not exist on device.
    pub image_path: String,
}

impl GameFile {
    pub fn display_name(&self) -> &str {
        self.name.strip_suffix(&format!(".{}", self.extension)).unwrap_or(&self.name)
    }
}

/// One entry from `favourite.json` (NDJSON — see `favorites` module).
///
/// Field names/order match the on-device JSON exactly; do not rename without
/// updating `#[serde(rename)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteGame {
    pub label: String,
    pub launch: String,
    #[serde(rename = "type")]
    pub kind: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imgpath: Option<String>,
    pub rompath: String,
}

impl FavoriteGame {
    /// Build a well-formed favourite entry for `game`, in the field layout
    /// the device expects (Quirk 1 — see `docs/DEVICE-PROTOCOL.md` §3).
    pub fn for_game(game: &GameFile) -> Self {
        let label = game.display_name().to_string();
        Self {
            launch: format!("/mnt/SDCARD/Emu/{}/launch.sh", game.console_folder),
            imgpath: Some(game.image_path.clone()),
            kind: 5,
            rompath: game.path.clone(),
            label,
        }
    }

    /// The rompath with `../../` segments resolved (Quirk 2), suitable for
    /// comparing against `GameFile::path` from a directory listing.
    pub fn normalized_path(&self) -> String {
        normalize_path(&self.rompath)
    }
}

/// One entry from `recentlist.json` / `recentlist-hidden.json` (also NDJSON).
/// Entries without a `rompath` are app launches, not games.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayHistoryEntry {
    pub label: String,
    pub launch: String,
    #[serde(rename = "type")]
    pub kind: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imgpath: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rompath: Option<String>,
}

impl PlayHistoryEntry {
    pub fn normalized_path(&self) -> Option<String> {
        self.rompath.as_deref().map(normalize_path)
    }
}

/// A save file (`.sav` / `.srm`) — distinct from a save *state*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveFile {
    pub name: String,
    pub game_name: String,
    pub path: String,
    pub size: Option<u64>,
}

/// A RetroArch save state, filed under `states/{core}/` (Quirk 3).
///
/// Serializable so the whole listing can go in `AppCache` — see the field
/// there for why an uncached listing made the feature look broken offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveState {
    pub name: String,
    pub path: String,
    pub game_name: String,
    /// `-1` for a plain `.sav`/`.srm` save file, `0` for `.state`/`.state.auto`
    /// (auto-save slot), `N` for `.state{N}`.
    pub slot_number: i32,
    pub size: Option<u64>,
    pub thumbnail_path: Option<String>,
}

impl SaveState {
    pub fn display_name(&self) -> String {
        match self.slot_number {
            -1 => "Save File".to_string(),
            0 => "Auto Save".to_string(),
            n => format!("Slot {n}"),
        }
    }
}

/// Cache-friendly, fully-owned representation of a console + its games,
/// written to `~/.local/state/emuhub/library.json` for offline browsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedConsole {
    pub folder: String,
    pub name: String,
    pub games: Vec<GameFile>,
}

/// Root on-disk cache structure.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppCache {
    pub consoles: Vec<CachedConsole>,
    /// Normalized ROM paths.
    pub favorites: Vec<String>,
    /// Play history, cached verbatim so the Recently Played list survives an
    /// offline launch. Stored as whole entries rather than bare paths on
    /// purpose: `recentlist.json` is rewritten on delete/rename, and writing
    /// back a half-synthesized entry would corrupt the device's list.
    #[serde(default)]
    pub recents: Vec<PlayHistoryEntry>,
    /// The whole save tree, cached for the same reason as `recents`: the
    /// listing only ever ran on connect, so an offline launch painted a full
    /// library in which every single game claimed to have no save states.
    #[serde(default)]
    pub save_states: Vec<SaveState>,
    /// When the last full library scan completed, as unix epoch seconds —
    /// deliberately not RFC3339, which would need date arithmetic (and a
    /// chrono dep) to produce and to turn back into "synced 2h ago". Rendered
    /// via `cache::relative_age`. Caches written before this field carried a
    /// value hold `null`, which still deserializes.
    pub last_full_sync: Option<u64>,
    #[serde(default = "default_cache_version")]
    pub version: u32,
}

fn default_cache_version() -> u32 {
    1
}

/// Device connection settings, persisted to `~/.config/emuhub/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_username")]
    pub username: String,
    /// Size cap for the on-disk box-art/thumbnail cache, in megabytes. The
    /// cache is pruned on exit (`cache::prune_image_cache`); without a cap it
    /// grows for the life of the install.
    #[serde(default = "default_image_cache_max_mb")]
    pub image_cache_max_mb: u64,
}

fn default_port() -> u16 {
    22
}

fn default_username() -> String {
    "root".to_string()
}

fn default_image_cache_max_mb() -> u64 {
    200
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: default_port(),
            username: default_username(),
            image_cache_max_mb: default_image_cache_max_mb(),
        }
    }
}
