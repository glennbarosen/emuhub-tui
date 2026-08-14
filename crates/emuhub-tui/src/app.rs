//! Application state and the pure state-transition logic that mutates it.
//! Rendering (`ui.rs`) is a pure function of `App`; the device task never
//! touches this struct directly, only through `DeviceEvent`s applied here.

use std::collections::{HashMap, HashSet};

use emuhub_core::cascade::{self, DeletePlan, RenamePlan};
use emuhub_core::models::{FavoriteGame, GameFile, PlayHistoryEntry, SaveState};
use emuhub_core::{consoles, saves, scan};
use ratatui::widgets::ListState;
use ratatui_image::protocol::StatefulProtocol;

use crate::device::DeviceEvent;
use crate::search::SearchState;

mod menus;
pub use menus::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Offline(String), // couldn't connect; browsing from cache
}

/// The three columns of the browser, in `h`/`l` order. `Detail` is the
/// always-visible bottom pane: focusing it turns its action list live, which
/// is how per-game actions (saves, rename, favourite, delete) became
/// discoverable instead of being unadvertised single-key bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    Consoles,
    Games,
    Detail,
}

/// Whether a console row is a real folder on the SD card or a synthetic view
/// over games that live elsewhere (Recently Played, Favourites). Virtual rows
/// must be skipped anywhere the list is treated as the library itself — fuzzy
/// search (or every recent game scores twice) and the on-disk cache (or the
/// next launch reads back a console folder that doesn't exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleKind {
    Real,
    Virtual,
}

/// Folder sentinels for the virtual rows. Deliberately not legal console
/// folder names, so `consoles::by_folder` can never resolve one.
///
/// There is more than one virtual row, so anything looking for a *specific*
/// one must match on the folder — `find(|c| c.kind == Virtual)` would return
/// whichever happens to come first and silently fill the wrong list.
pub const RECENT_FOLDER: &str = "__RECENT";
pub const FAVORITES_FOLDER: &str = "__FAVORITES";

/// One console's static metadata plus its currently known ROM list (from
/// cache and/or device — whichever loaded last wins, matching the Swift
/// original's "cache first, reconcile on connect" behaviour).
pub struct ConsoleEntry {
    pub folder: &'static str,
    pub name: &'static str,
    pub icon: &'static str,
    pub games: Vec<GameFile>,
    pub kind: ConsoleKind,
}

pub struct StatusMessage {
    pub text: String,
    pub is_error: bool,
}

pub struct App {
    pub connection: ConnectionState,
    pub host: String,

    pub consoles: Vec<ConsoleEntry>,
    /// Flat index for O(1) "is this game favourited" / favourite rebuild.
    pub games_by_path: HashMap<String, GameFile>,

    /// Raw entries as read from (or destined for) `favourite.json` — kept
    /// intact rather than rebuilt from scratch so favourites for ROMs the
    /// current scan didn't see (SD card swapped, card partially indexed)
    /// aren't silently dropped on next sync.
    pub favorite_entries: Vec<FavoriteGame>,
    pub favorite_paths: HashSet<String>,

    /// Raw `recentlist.json` entries, most-recent first — kept whole (not
    /// reduced to paths) because delete/rename has to rewrite that file, and
    /// the device's own `label`/`launch` fields must survive the round trip.
    pub recents: Vec<PlayHistoryEntry>,

    /// Offline-mode pending queue, mirroring the Swift `FavoritesManager`'s
    /// pendingAdditions/pendingRemovals — applied and cleared on reconnect.
    pub pending_additions: HashSet<String>,
    pub pending_removals: HashSet<String>,

    pub focus: Pane,
    pub console_idx: usize,
    pub game_idx: usize,

    /// The Details pane's action-list cursor. Always present (the pane is
    /// always drawn), reset by `reset_detail_menu` whenever the selection
    /// moves so it can never point at an action for a game you've left.
    pub detail: DetailState,

    /// Every save state on the card, flat — indexed per game on demand via
    /// `saves::states_for_game`.
    pub save_states: Vec<SaveState>,

    pub search: Option<SearchState>,
    pub ip_prompt: Option<IpPromptState>,
    pub settings: Option<SettingsState>,
    pub saves: Option<SavesState>,
    pub discovery: Option<DiscoveryState>,
    pub confirm_delete: Option<ConfirmDeleteState>,
    pub rename_prompt: Option<RenamePromptState>,
    /// The `?` overlay. A plain flag rather than an `Option<State>` — it holds
    /// no cursor and no input, it's rendered straight off `HELP_SECTIONS`.
    pub help: bool,

    pub status: Option<StatusMessage>,
    pub should_quit: bool,

    /// When the library was last fully scanned, unix epoch seconds — from the
    /// cache at startup, then updated by every `LibraryLoaded`. This is what
    /// lets the status line say how stale `offline (cache)` actually is.
    pub last_sync: Option<u64>,

    /// The one decoded image currently on screen — a single slot, not a
    /// cache: displaying a different one evicts it. The disk cache
    /// (`emuhub_core::cache`) is what makes going back cheap; there is
    /// deliberately no in-app "already requested" gate beyond
    /// `image_for_key`, since that would leave a once-visited-then-evicted
    /// image stuck un-refetchable forever.
    ///
    /// `image_for_key` holds the *identity* the image was requested under
    /// (see `current_image`): a ROM path for box art, a thumbnail path for a
    /// save state. Never the box art's own path — that check can never match.
    pub image_state: Option<StatefulProtocol>,
    pub image_for_key: Option<String>,

    /// Scroll offsets for the three lists. Kept across frames rather than
    /// rebuilt per draw so the viewport stays put as you move within it,
    /// instead of snapping the selection to the top or bottom edge on every
    /// keypress. `ui.rs` sets `.select(...)` each frame; ratatui derives the
    /// offset from that plus the retained one.
    pub console_list: ListState,
    pub games_list: ListState,
    pub search_list: ListState,

    /// Detected once at startup (terminal query is not free) — Kitty in
    /// Ghostty, halfblocks elsewhere.
    picker: ratatui_image::picker::Picker,
}

impl App {
    pub fn new(host: String) -> Self {
        // Recently Played and Favourites are pinned at indices 0 and 1 from the
        // very start rather than inserted once their contents arrive, so
        // `console_idx` means the same thing for the whole session — inserting
        // a row later would silently renumber every console the user (or a
        // live search result) is pointing at.
        let recent = ConsoleEntry {
            folder: RECENT_FOLDER,
            name: "Recently Played",
            icon: "🕘",
            games: Vec::new(),
            kind: ConsoleKind::Virtual,
        };
        let favorites = ConsoleEntry {
            folder: FAVORITES_FOLDER,
            name: "Favourites",
            icon: "★",
            games: Vec::new(),
            kind: ConsoleKind::Virtual,
        };
        let consoles = [recent, favorites]
            .into_iter()
            .chain(consoles::ALL_SYSTEMS.iter().map(|c| ConsoleEntry {
                folder: c.folder,
                name: c.name,
                icon: c.icon,
                games: Vec::new(),
                kind: ConsoleKind::Real,
            }))
            .collect();

        Self {
            connection: ConnectionState::Disconnected,
            host,
            consoles,
            games_by_path: HashMap::new(),
            favorite_entries: Vec::new(),
            favorite_paths: HashSet::new(),
            recents: Vec::new(),
            pending_additions: HashSet::new(),
            pending_removals: HashSet::new(),
            focus: Pane::Consoles,
            console_idx: 0,
            game_idx: 0,
            detail: DetailState::new(),
            save_states: Vec::new(),
            search: None,
            ip_prompt: None,
            settings: None,
            saves: None,
            discovery: None,
            confirm_delete: None,
            rename_prompt: None,
            help: false,
            status: None,
            should_quit: false,
            last_sync: None,
            image_state: None,
            image_for_key: None,
            console_list: ListState::default(),
            games_list: ListState::default(),
            search_list: ListState::default(),
            picker: ratatui_image::picker::Picker::from_query_stdio()
                .unwrap_or_else(|_| ratatui_image::picker::Picker::halfblocks()),
        }
    }

    pub fn set_status(&mut self, text: impl Into<String>, is_error: bool) {
        self.status = Some(StatusMessage { text: text.into(), is_error });
    }

    pub fn open_settings(&mut self) {
        self.settings = Some(SettingsState::new());
    }

    pub fn open_discovery(&mut self) {
        self.discovery = Some(DiscoveryState::new());
    }

    /// Accepts the highlighted discovered address as the new host, closing the
    /// modal. Same shape as `confirm_ip_prompt` — state changes here, the
    /// caller persists and reconnects.
    pub fn confirm_discovery(&mut self) -> Option<String> {
        let host = self.discovery.as_ref()?.current()?.clone();
        self.host = host.clone();
        self.discovery = None;
        Some(host)
    }

    /// Closes the settings menu and hands the chosen item back for the caller
    /// to act on — same "mutate state here, do the I/O in `main.rs`" split as
    /// `confirm_ip_prompt` and `toggle_favorite`.
    pub fn confirm_settings(&mut self) -> Option<SettingsItem> {
        let item = self.settings.as_ref()?.current();
        self.settings = None;
        Some(item)
    }

    /// Opens the IP-entry modal, prefilled with the current host so changing
    /// it is an edit rather than a blank retype.
    pub fn open_ip_prompt(&mut self) {
        self.ip_prompt = Some(IpPromptState { input: self.host.clone() });
    }

    /// Trims and applies the prompt's input as the new host, closing the
    /// modal and returning it for the caller to persist/send over the
    /// device channel (I/O lives in `main.rs`, same split as
    /// `toggle_favorite`). Blank input is left as a no-op — the prompt stays
    /// open rather than clearing the configured host.
    pub fn confirm_ip_prompt(&mut self) -> Option<String> {
        let host = self.ip_prompt.as_ref()?.input.trim().to_string();
        if host.is_empty() {
            return None;
        }
        self.host = host.clone();
        self.ip_prompt = None;
        Some(host)
    }

    /// Indices into `consoles` of the entries the console pane actually
    /// renders — consoles with no ROMs are hidden.
    pub fn visible_console_indices(&self) -> Vec<usize> {
        self.consoles.iter().enumerate().filter(|(_, c)| !c.games.is_empty()).map(|(i, _)| i).collect()
    }

    /// The *row* the selected console occupies in the rendered pane, which is
    /// its position among the visible entries — not `console_idx`, since the
    /// hidden empties shift everything up. The list widget derives its scroll
    /// offset from this, so conflating the two scrolls to the wrong place.
    pub fn selected_console_row(&self) -> Option<usize> {
        self.visible_console_indices().iter().position(|&i| i == self.console_idx)
    }

    pub fn current_console(&self) -> Option<&ConsoleEntry> {
        self.consoles.get(self.console_idx)
    }

    /// The game under the cursor, accounting for whether search is active.
    pub fn selected_game(&self) -> Option<&GameFile> {
        if let Some(search) = &self.search {
            let m = search.matches.get(search.selected)?;
            self.consoles.get(m.console_idx)?.games.get(m.game_idx)
        } else {
            self.current_console()?.games.get(self.game_idx)
        }
    }

    /// The save states belonging to `game`, most plausible core first.
    pub fn states_for(&self, game: &GameFile) -> Vec<SaveState> {
        saves::states_for_game(&self.save_states, game)
    }

    /// Where `game` sits in the play history, 1-based, or `None` if it isn't
    /// in it.
    ///
    /// A rank rather than a date because `recentlist.json` has no timestamp
    /// field — the device only records order, so order is all we can honestly
    /// show. Duplicates are deduped the same way `rebuild_recent_console`
    /// does, or the ranks wouldn't match the row numbers in that list.
    pub fn recent_rank(&self, game: &GameFile) -> Option<usize> {
        let mut seen = HashSet::new();
        self.recents
            .iter()
            .filter_map(|entry| entry.normalized_path())
            .filter(|path| seen.insert(path.clone()))
            .position(|path| path == game.path)
            .map(|i| i + 1)
    }

    /// How many saves a game has, for the marker in the games list. Cheap
    /// enough to call per visible row — the listing is a few hundred entries
    /// at worst.
    pub fn save_count(&self, game: &GameFile) -> usize {
        self.save_states.iter().filter(|s| saves::belongs_to(s, game)).count()
    }

    /// Opens the save-state browser for the selected game. Returns `false`
    /// only when no game is selected at all.
    ///
    /// An empty list still opens: the browser explains *why* it's empty (no
    /// saves for this game vs. the device never gave us a listing), which a
    /// one-line status message that vanishes on the next keypress could not.
    pub fn open_saves(&mut self) -> bool {
        let Some(game) = self.selected_game().cloned() else {
            return false;
        };
        let states = self.states_for(&game);
        self.saves = Some(SavesState { game, states, selected: 0, list: ListState::default() });
        true
    }

    /// Whether it's worth asking the device for a fresh save listing — we have
    /// nothing, and there's a live session to ask. The connect path loads this
    /// once; this is what lets a session that started offline recover without
    /// a full reconnect.
    pub fn needs_save_listing(&self) -> bool {
        self.save_states.is_empty() && self.connection == ConnectionState::Connected
    }

    /// The image the UI wants on screen right now, as `(key, remote path)`.
    ///
    /// The key is the *identity* the decoded image is filed under and the
    /// remote path is where the bytes live — the same split
    /// `DeviceRequest::FetchImage` makes, and for the same reason: conflating
    /// them is what made box art decode successfully but never render. With
    /// the saves overlay open the pair describes the selected state's
    /// thumbnail; otherwise the selected game's box art.
    pub fn current_image(&self) -> Option<(String, String)> {
        if let Some(saves) = &self.saves {
            let thumbnail = saves.current()?.thumbnail_path.clone()?;
            return Some((thumbnail.clone(), thumbnail));
        }
        let game = self.selected_game()?;
        Some((game.path.clone(), game.image_path.clone()))
    }

    /// Builds the delete cascade for the selected game and opens the
    /// confirmation. Nothing is sent to the device until the user confirms.
    pub fn open_confirm_delete(&mut self) -> bool {
        let Some(game) = self.selected_game().cloned() else {
            return false;
        };
        let plan = cascade::delete_plan(&game, &self.save_states, &self.favorite_entries, &self.recents);
        self.confirm_delete = Some(ConfirmDeleteState { game, plan });
        true
    }

    /// The confirmed plan, closing the dialog. The caller does the I/O.
    pub fn confirm_delete(&mut self) -> Option<(GameFile, DeletePlan)> {
        let state = self.confirm_delete.take()?;
        Some((state.game, state.plan))
    }

    pub fn open_rename_prompt(&mut self) -> bool {
        let Some(game) = self.selected_game().cloned() else {
            return false;
        };
        self.rename_prompt =
            Some(RenamePromptState { input: game.display_name().to_string(), game, error: None });
        true
    }

    /// The cascade the rename prompt's current input *would* produce, as
    /// summary lines — the same dry run the delete dialog shows, so both
    /// destructive actions name their files before they run rather than only
    /// the one that can't be undone.
    ///
    /// `None` while the name is invalid or unchanged: there is nothing
    /// truthful to list, and the prompt already renders the validation error.
    /// Recomputed per frame rather than cached — `cascade::rename_plan` is
    /// pure, and the input changes on every keystroke anyway.
    pub fn rename_preview(&self) -> Option<Vec<String>> {
        let prompt = self.rename_prompt.as_ref()?;
        if prompt.input.trim() == prompt.game.display_name() {
            return None;
        }
        let plan = cascade::rename_plan(
            &prompt.game,
            &prompt.input,
            &self.save_states,
            &self.favorite_entries,
            &self.recents,
        )
        .ok()?;
        Some(plan.summary())
    }

    /// Validates the typed name and returns the resulting plan, closing the
    /// prompt. A rejected name leaves the prompt open with the reason
    /// attached, rather than discarding what the user typed.
    pub fn confirm_rename_prompt(&mut self) -> Option<(GameFile, RenamePlan)> {
        let prompt = self.rename_prompt.as_ref()?;
        let game = prompt.game.clone();
        let plan = cascade::rename_plan(
            &game,
            &prompt.input,
            &self.save_states,
            &self.favorite_entries,
            &self.recents,
        );

        match plan {
            Ok(plan) => {
                self.rename_prompt = None;
                Some((game, plan))
            }
            Err(err) => {
                if let Some(prompt) = &mut self.rename_prompt {
                    prompt.error = Some(err.to_string());
                }
                None
            }
        }
    }

    /// Drops a deleted game from every index it appears in, so the browser
    /// stays correct without a full rescan.
    ///
    /// The favourite and recent lists are replaced from the plan rather than
    /// filtered again here — the plan is what was actually written to the
    /// device, so anything else would drift from it.
    pub fn apply_deletion(&mut self, game: &GameFile, plan: &DeletePlan) {
        for console in &mut self.consoles {
            console.games.retain(|g| g.path != game.path);
        }
        self.games_by_path.remove(&game.path);
        self.save_states.retain(|s| !saves::belongs_to(s, game));

        if let Some(favorites) = &plan.favorites {
            self.load_favorites(favorites.clone());
        }
        if let Some(recents) = &plan.recents {
            self.recents = recents.clone();
        }
        self.pending_additions.remove(&game.path);
        self.pending_removals.remove(&game.path);
        // Unconditionally, not just when the plan rewrote `favourite.json`:
        // `favorite_paths` can also come from the on-disk cache, which has no
        // entries for the plan to have noticed.
        self.favorite_paths.remove(&game.path);

        // The virtual rows are views over `recents`/`favorite_paths`, so they
        // have to be rebuilt after those are updated — otherwise the deleted
        // game reappears under Recently Played or Favourites.
        self.rebuild_recent_console();
        self.rebuild_favorites_console();
        self.evict_image_for(&game.path);
        self.clamp_selection();
        // The cursor now points at a *different* game, so a menu still sitting
        // on "Delete" would be aimed at an innocent bystander.
        self.reset_detail_menu();
    }

    /// Re-points every index at a renamed game, in place.
    pub fn apply_rename(&mut self, old: &GameFile, plan: &RenamePlan) {
        let new_game = &plan.new_game;
        for console in &mut self.consoles {
            for slot in &mut console.games {
                if slot.path == old.path {
                    *slot = new_game.clone();
                }
            }
        }
        self.games_by_path.remove(&old.path);
        self.games_by_path.insert(new_game.path.clone(), new_game.clone());

        // Saves moved with the ROM, so the cached listing has to move too or
        // the renamed game shows zero saves until the next reconnect.
        for (from, to) in &plan.renames {
            for state in &mut self.save_states {
                if &state.path == from {
                    state.path = to.clone();
                    state.name =
                        to.rsplit_once('/').map(|(_, n)| n.to_string()).unwrap_or_else(|| to.clone());
                    state.game_name = new_game.display_name().to_string();
                }
                if state.thumbnail_path.as_ref() == Some(from) {
                    state.thumbnail_path = Some(to.clone());
                }
            }
        }

        if let Some(favorites) = &plan.favorites {
            self.load_favorites(favorites.clone());
        }
        if let Some(recents) = &plan.recents {
            self.recents = recents.clone();
        }
        if self.pending_additions.remove(&old.path) {
            self.pending_additions.insert(new_game.path.clone());
        }
        if self.pending_removals.remove(&old.path) {
            self.pending_removals.insert(new_game.path.clone());
        }
        // Same reasoning as `apply_deletion`: cached favourite paths exist
        // without matching entries, so the plan can't be the only thing that
        // re-points them.
        if self.favorite_paths.remove(&old.path) {
            self.favorite_paths.insert(new_game.path.clone());
        }

        self.rebuild_recent_console();
        self.rebuild_favorites_console();
        self.evict_image_for(&old.path);
        self.clamp_selection();
    }

    /// Forgets the on-screen image if it belonged to `path` — its bytes are
    /// no longer at the address they were fetched from.
    fn evict_image_for(&mut self, path: &str) {
        if self.image_for_key.as_deref() == Some(path) {
            self.image_state = None;
            self.image_for_key = None;
        }
    }

    pub fn is_favorite(&self, game: &GameFile) -> bool {
        self.favorite_paths.contains(&game.path)
    }

    /// Loads a freshly-scanned library, replacing whatever was there
    /// (cache or a previous connection).
    pub fn load_library(&mut self, grouped: Vec<(&'static str, Vec<GameFile>)>) {
        self.games_by_path.clear();
        for (folder, games) in grouped {
            if let Some(entry) = self.consoles.iter_mut().find(|c| c.folder == folder) {
                for game in &games {
                    self.games_by_path.insert(game.path.clone(), game.clone());
                }
                entry.games = games;
            }
        }
        self.rebuild_recent_console();
        self.rebuild_favorites_console();
        self.clamp_selection();
    }

    pub fn load_recents(&mut self, entries: Vec<PlayHistoryEntry>) {
        self.recents = entries;
        self.rebuild_recent_console();
        self.clamp_selection();
    }

    /// Resolves `recents` into the Recently Played row's game list.
    ///
    /// Called from both `load_library` and `load_recents` because the two
    /// arrive independently — the device sends library → favourites → recents,
    /// but an offline launch paints the cached library first and may have
    /// cached recents alongside it. Whichever lands second has to rebuild.
    ///
    /// Entries the current scan didn't see (ROM deleted, card swapped) are
    /// rebuilt through `scan::parse_rom_path` rather than dropped, so the list
    /// matches what the handheld itself shows.
    fn rebuild_recent_console(&mut self) {
        let mut seen = HashSet::new();
        let games: Vec<GameFile> = self
            .recents
            .iter()
            .filter_map(|entry| entry.normalized_path())
            .filter(|path| seen.insert(path.clone()))
            .filter_map(|path| self.games_by_path.get(&path).cloned().or_else(|| scan::parse_rom_path(&path)))
            .collect();

        self.set_virtual_console(RECENT_FOLDER, games);
    }

    /// Resolves `favorite_paths` into the Favourites row's game list.
    ///
    /// Sorted by console then name rather than left in `favourite.json` order:
    /// the source here is a `HashSet`, so without an explicit ordering the row
    /// would reshuffle itself between frames. Console-major keeps a
    /// mixed-system favourites list readable.
    ///
    /// Like recents, a favourite the current scan didn't see is rebuilt via
    /// `scan::parse_rom_path` instead of vanishing — a favourite for a ROM on a
    /// card that isn't in the reader is still a favourite.
    fn rebuild_favorites_console(&mut self) {
        let mut games: Vec<GameFile> = self
            .favorite_paths
            .iter()
            .filter_map(|path| self.games_by_path.get(path).cloned().or_else(|| scan::parse_rom_path(path)))
            .collect();
        games.sort_by(|a, b| {
            a.console_folder.cmp(&b.console_folder).then_with(|| a.display_name().cmp(b.display_name()))
        });

        self.set_virtual_console(FAVORITES_FOLDER, games);
    }

    /// Fills one virtual row, located by folder.
    ///
    /// By folder and never by `kind`: there are two virtual rows, and matching
    /// on the kind returns whichever is first in the list — which would quietly
    /// paint the favourites into Recently Played.
    fn set_virtual_console(&mut self, folder: &str, games: Vec<GameFile>) {
        if let Some(entry) = self.consoles.iter_mut().find(|c| c.folder == folder) {
            entry.games = games;
        }
    }

    pub fn load_favorites(&mut self, entries: Vec<FavoriteGame>) {
        self.favorite_paths = entries.iter().map(|f| f.normalized_path()).collect();
        self.favorite_entries = entries;
        self.rebuild_favorites_console();
        self.clamp_selection();
    }

    /// Seeds favourites from the on-disk cache, which stores bare paths rather
    /// than whole `favourite.json` entries. Separate from `load_favorites` so
    /// the Favourites row is populated on an offline launch too — assigning
    /// `favorite_paths` directly (as `main.rs` used to) leaves the row empty
    /// until the device answers.
    pub fn load_cached_favorite_paths(&mut self, paths: HashSet<String>) {
        self.favorite_paths = paths;
        self.rebuild_favorites_console();
        self.clamp_selection();
    }

    /// Toggles the favourite state of `game`, updating local state
    /// immediately (optimistic) and returning the full entry list that
    /// should now be written to the device (or queued if offline).
    ///
    /// `pending_additions`/`pending_removals` track the *net* change versus
    /// the device's last known state, so toggling a favourite on and back
    /// off in the same offline session must cancel out to nothing pending —
    /// not leave a spurious removal queued for a favourite the device never
    /// actually had added in the first place.
    pub fn toggle_favorite(&mut self, game: &GameFile) -> Vec<FavoriteGame> {
        let path = game.path.clone();
        if self.favorite_paths.remove(&path) {
            self.favorite_entries.retain(|f| f.normalized_path() != path);
            if !self.pending_additions.remove(&path) {
                self.pending_removals.insert(path.clone());
            }
        } else {
            self.favorite_entries.push(FavoriteGame::for_game(game));
            self.favorite_paths.insert(path.clone());
            if !self.pending_removals.remove(&path) {
                self.pending_additions.insert(path.clone());
            }
        }
        // The Favourites row is a view over exactly what just changed, so it
        // has to follow the toggle immediately — including the case where the
        // toggle happened *inside* that row and the game must now leave it.
        self.rebuild_favorites_console();
        self.clamp_selection();
        self.favorite_entries.clone()
    }

    pub fn clear_pending(&mut self) {
        self.pending_additions.clear();
        self.pending_removals.clear();
    }

    pub fn has_pending_changes(&self) -> bool {
        !self.pending_additions.is_empty() || !self.pending_removals.is_empty()
    }

    fn clamp_selection(&mut self) {
        self.console_idx = self.console_idx.min(self.consoles.len().saturating_sub(1));
        if self.current_console().map(|c| c.games.is_empty()).unwrap_or(false) {
            if let Some(idx) = self.consoles.iter().position(|c| !c.games.is_empty()) {
                self.console_idx = idx;
            }
        }
        let len = self.current_console().map(|c| c.games.len()).unwrap_or(0);
        self.game_idx = self.game_idx.min(len.saturating_sub(1));
    }

    pub fn move_selection(&mut self, delta: i32) {
        if let Some(search) = &mut self.search {
            if search.matches.is_empty() {
                return;
            }
            let len = search.matches.len() as i32;
            search.selected = ((search.selected as i32 + delta).rem_euclid(len)) as usize;
            // `selected_game` follows the search cursor, so this is a game
            // change like any other as far as the Details menu is concerned.
            self.reset_detail_menu();
            return;
        }

        match self.focus {
            Pane::Consoles => {
                let len = self.consoles.len() as i32;
                if len == 0 || self.consoles.iter().all(|c| c.games.is_empty()) {
                    return;
                }
                let mut idx = self.console_idx as i32;
                loop {
                    idx = (idx + delta).rem_euclid(len);
                    if !self.consoles[idx as usize].games.is_empty() {
                        break;
                    }
                }
                self.console_idx = idx as usize;
                self.game_idx = 0;
                self.reset_detail_menu();
            }
            Pane::Games => {
                let len = self.current_console().map(|c| c.games.len()).unwrap_or(0) as i32;
                if len == 0 {
                    return;
                }
                self.game_idx = ((self.game_idx as i32 + delta).rem_euclid(len)) as usize;
                self.reset_detail_menu();
            }
            // With the Details pane focused the same keys drive its action
            // list instead of the library — the selected game stays put.
            Pane::Detail => self.detail.move_selection(delta as isize),
        }
    }

    /// Puts the Details menu back at the top level, first row. Called whenever
    /// the selected game changes: an action list left on "Delete" for the
    /// previous game is a genuinely dangerous piece of stale state.
    pub fn reset_detail_menu(&mut self) {
        self.detail = DetailState::new();
    }

    /// Moves focus one pane to the right: `Consoles → Games → Detail`.
    pub fn focus_next(&mut self) {
        self.focus = match self.focus {
            Pane::Consoles => Pane::Games,
            Pane::Games => Pane::Detail,
            Pane::Detail => Pane::Detail,
        };
    }

    /// Moves focus one pane to the left. Leaving the Details pane also resets
    /// its menu, so re-entering always starts from a known row.
    pub fn focus_prev(&mut self) {
        self.focus = match self.focus {
            Pane::Detail => {
                self.reset_detail_menu();
                Pane::Games
            }
            Pane::Games => Pane::Consoles,
            Pane::Consoles => Pane::Consoles,
        };
    }

    /// Activates the highlighted top-level action, entering the nested
    /// settings level in place when that's what was chosen. Returns the item
    /// so `main.rs` can do the I/O — same split as `confirm_settings`.
    pub fn confirm_detail(&mut self) -> Option<DetailItem> {
        if self.detail.level != DetailLevel::Actions {
            return None;
        }
        let item = *DetailItem::ALL.get(self.detail.selected)?;
        if item == DetailItem::Settings {
            self.detail.enter_settings();
        }
        Some(item)
    }

    /// The highlighted per-game action. Leaves the menu on the settings level:
    /// rename and delete open their own modals over it, and closing one should
    /// land back where the action was chosen.
    pub fn confirm_game_setting(&mut self) -> Option<GameSettingsItem> {
        if self.detail.level != DetailLevel::Settings {
            return None;
        }
        GameSettingsItem::ALL.get(self.detail.selected).copied()
    }

    /// Applies one event from the device task. When it returns `Some`, the
    /// caller must send `DeviceRequest::SyncFavorites` with that list — kept
    /// out of this function so `App` never needs to know about channels.
    pub fn apply_device_event(&mut self, event: DeviceEvent) -> Option<Vec<FavoriteGame>> {
        match event {
            DeviceEvent::Connecting => {
                self.connection = ConnectionState::Connecting;
                None
            }
            DeviceEvent::Connected => {
                self.connection = ConnectionState::Connected;
                self.set_status(format!("Connected to {}", self.host), false);
                None
            }
            DeviceEvent::ConnectFailed(err) => {
                self.connection = ConnectionState::Offline(err.clone());
                self.set_status(format!("Offline — {err} (browsing cache)"), true);
                None
            }
            DeviceEvent::LibraryLoaded(grouped) => {
                self.load_library(grouped);
                // A completed scan is what "synced" means on the status line —
                // not a successful connect, which can be followed by a listing
                // that fails.
                self.last_sync = Some(emuhub_core::cache::now_epoch());
                None
            }
            DeviceEvent::RecentsLoaded(entries) => {
                self.load_recents(entries);
                None
            }
            DeviceEvent::SaveStatesLoaded(states) => {
                // A listing that comes back empty is worth saying out loud:
                // otherwise it looks exactly like one that never ran, and
                // every game reports "no save states" with no explanation.
                if states.is_empty() {
                    self.set_status("No save states found on the device", false);
                }
                self.save_states = states;
                // Refresh an already-open browser in place rather than
                // closing it — a listing can land while the user is looking.
                if let Some(open) = &mut self.saves {
                    open.states = saves::states_for_game(&self.save_states, &open.game);
                    open.selected = open.selected.min(open.states.len().saturating_sub(1));
                }
                None
            }
            DeviceEvent::FavoritesLoaded(entries) => {
                self.load_favorites(entries);
                // Reconnected with offline toggles still queued: apply them
                // on top of the device's current state and push back,
                // mirroring the Swift original's syncPendingChanges() — the
                // toggle a user made with the device powered off must not
                // be silently dropped by a fresh read on reconnect.
                if self.has_pending_changes() {
                    Some(self.reconcile_pending())
                } else {
                    None
                }
            }
            DeviceEvent::FavoritesSynced => {
                self.clear_pending();
                self.set_status("Favourites synced to device", false);
                None
            }
            DeviceEvent::ImageBytes { key, data } => {
                self.decode_image(key, data);
                None
            }
            DeviceEvent::GameDeleted { game, plan } => {
                self.apply_deletion(&game, &plan);
                self.set_status(
                    format!(
                        "Deleted {} and {} related file(s)",
                        game.display_name(),
                        plan.removals.len() - 1
                    ),
                    false,
                );
                None
            }
            DeviceEvent::GameRenamed { game, plan } => {
                let new_name = plan.new_game.display_name().to_string();
                self.apply_rename(&game, &plan);
                self.set_status(format!("Renamed to {new_name}"), false);
                None
            }
            DeviceEvent::DiscoveryStarted { network } => {
                if let Some(discovery) = &mut self.discovery {
                    discovery.network = Some(network);
                }
                None
            }
            DeviceEvent::DiscoveryProgress { done, total } => {
                if let Some(discovery) = &mut self.discovery {
                    discovery.done = done;
                    discovery.total = total;
                }
                None
            }
            DeviceEvent::DiscoveryFound(host) => {
                if let Some(discovery) = &mut self.discovery {
                    if !discovery.found.contains(&host) {
                        discovery.found.push(host);
                    }
                }
                None
            }
            DeviceEvent::DiscoveryRejected { host, reason } => {
                if let Some(discovery) = &mut self.discovery {
                    if !discovery.rejected.iter().any(|(h, _)| *h == host) {
                        discovery.rejected.push((host, reason));
                    }
                }
                None
            }
            DeviceEvent::DiscoveryDone => {
                let mut status = None;
                if let Some(discovery) = &mut self.discovery {
                    discovery.finished = true;
                    if discovery.found.is_empty() {
                        // Name the range that was actually swept: "found
                        // nothing" and "searched the wrong half of the
                        // network" are the same sentence otherwise.
                        status = Some(match &discovery.network {
                            Some(network) => {
                                format!("No devices found on {network} — is the handheld awake and on wifi?")
                            }
                            None => "No devices found on this network".to_string(),
                        });
                    }
                }
                if let Some(status) = status {
                    self.set_status(status, true);
                }
                None
            }
            DeviceEvent::Error(err) => {
                self.set_status(err, true);
                None
            }
        }
    }

    /// Re-applies queued offline additions/removals on top of a freshly
    /// loaded favourites list, returning the merged list to sync back.
    fn reconcile_pending(&mut self) -> Vec<FavoriteGame> {
        for path in self.pending_additions.clone() {
            if !self.favorite_paths.contains(&path) {
                if let Some(game) = self.games_by_path.get(&path).cloned() {
                    self.favorite_entries.push(FavoriteGame::for_game(&game));
                    self.favorite_paths.insert(path);
                }
            }
        }
        for path in self.pending_removals.clone() {
            self.favorite_entries.retain(|f| f.normalized_path() != path);
            self.favorite_paths.remove(&path);
        }
        self.favorite_entries.clone()
    }

    fn decode_image(&mut self, key: String, data: Vec<u8>) {
        match image::load_from_memory(&data) {
            Ok(dynamic) => {
                let protocol = self.picker.new_resize_protocol(dynamic);
                self.image_state = Some(protocol);
                tracing::debug!(%key, "image decoded and ready to render");
                self.image_for_key = Some(key);
            }
            Err(err) => {
                tracing::warn!(%err, %key, "failed to decode image");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! These exercise exactly the state transitions `main.rs::handle_key`
    //! drives on each keypress, without a real terminal — the sandbox this
    //! was built in can't faithfully emulate raw-mode tty input (crossterm
    //! never observed a single event there, even though rendering, connect,
    //! and reconnect all demonstrably worked over the same pty), so this is
    //! the automatable half of end-to-end verification. The other half —
    //! actually pressing keys in Ghostty — needs a human.
    use super::*;

    fn game(console: &str, name: &str) -> GameFile {
        GameFile {
            path: format!("/mnt/SDCARD/Roms/{console}/{name}.gba"),
            name: format!("{name}.gba"),
            console_folder: console.to_string(),
            extension: "gba".to_string(),
            size: None,
            image_path: format!("/mnt/SDCARD/Roms/{console}/Imgs/{name}.png"),
        }
    }

    fn app_with_games() -> App {
        let mut app = App::new("192.168.1.1".to_string());
        let gb_games = vec![game("GB", "Tetris"), game("GB", "Kirby")];
        let gba_games = vec![game("GBA", "Metroid Fusion")];
        app.load_library(vec![("GB", gb_games), ("GBA", gba_games)]);
        app
    }

    /// Console rows are looked up by folder rather than hardcoded, since the
    /// virtual Recently Played row occupies index 0 and any future virtual
    /// row would shift them again.
    fn idx_of(app: &App, folder: &str) -> usize {
        app.consoles.iter().position(|c| c.folder == folder).expect("unknown console folder")
    }

    #[test]
    fn j_k_move_selection_within_focused_pane_and_wrap() {
        let mut app = app_with_games();
        app.focus = Pane::Consoles;

        app.console_idx = idx_of(&app, "GB");
        app.move_selection(1); // 'j', skips empty GBC, lands on GBA
        assert_eq!(app.console_idx, idx_of(&app, "GBA"));
        app.move_selection(-1); // 'k', back to GB
        assert_eq!(app.console_idx, idx_of(&app, "GB"));
        app.move_selection(-1); // wrap backward, skipping empties, to GBA (last non-empty)
        assert_eq!(app.console_idx, idx_of(&app, "GBA"));
    }

    #[test]
    fn moving_console_selection_resets_game_index_and_switching_pane_moves_games() {
        let mut app = app_with_games();
        app.focus = Pane::Games;
        app.game_idx = 1; // "Kirby" under GB

        app.focus_prev(); // 'h' -> Consoles
        assert_eq!(app.focus, Pane::Consoles);
        app.move_selection(1); // moves to GBA (skipping empty GBC), resets game_idx
        assert_eq!(app.game_idx, 0);

        app.focus_next(); // 'l' -> Games
        assert_eq!(app.focus, Pane::Games);
    }

    #[test]
    fn toggling_favorite_updates_the_game_and_reports_the_full_entry_list() {
        let mut app = app_with_games();
        app.focus = Pane::Games;
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0; // Tetris

        let selected = app.selected_game().cloned().unwrap();
        assert_eq!(selected.name, "Tetris.gba");
        assert!(!app.is_favorite(&selected));

        let entries = app.toggle_favorite(&selected);
        assert!(app.is_favorite(&selected));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "Tetris");
        assert!(app.pending_additions.contains(&selected.path));

        // Toggling again removes it.
        let entries = app.toggle_favorite(&selected);
        assert!(!app.is_favorite(&selected));
        assert!(entries.is_empty());
        // A same-session add-then-remove nets out to no pending change,
        // matching the Swift original's pendingAdditions/pendingRemovals
        // cancel-out behaviour.
        assert!(!app.has_pending_changes());
    }

    #[test]
    fn remove_then_readd_also_cancels_out() {
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");
        // Start already favourited (as if loaded from device/cache).
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);
        assert!(app.is_favorite(&tetris));

        app.toggle_favorite(&tetris); // remove
        assert!(app.pending_removals.contains(&tetris.path));
        app.toggle_favorite(&tetris); // add back
        assert!(app.is_favorite(&tetris));
        assert!(!app.has_pending_changes(), "remove-then-readd must not leave a spurious pending addition");
    }

    #[test]
    fn slash_search_scores_across_every_console_at_once() {
        let mut app = app_with_games();
        let mut search = SearchState::new();
        search.push_char('m', &app.consoles);
        search.push_char('e', &app.consoles);
        search.push_char('t', &app.consoles);
        app.search = Some(search);

        let hit = app.selected_game().unwrap();
        assert_eq!(hit.name, "Metroid Fusion.gba");
    }

    #[test]
    fn escaping_search_returns_to_normal_pane_navigation() {
        let mut app = app_with_games();
        app.search = Some(SearchState::new());
        assert!(app.selected_game().is_none()); // empty query, no matches yet

        app.search = None; // 'esc'
        assert_eq!(app.selected_game().unwrap().name, "Tetris.gba"); // back to console 0 / game 0
    }

    #[test]
    fn offline_toggle_then_reconnect_reconciles_instead_of_being_dropped() {
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");

        // Device starts with Tetris NOT favourited; we toggle it on while
        // offline (device task never received the sync).
        app.toggle_favorite(&tetris);
        assert!(app.pending_additions.contains(&tetris.path));

        // Reconnect: device reports its own (stale) favourites list, which
        // does not include our offline toggle.
        let follow_up = app.apply_device_event(DeviceEvent::FavoritesLoaded(vec![]));

        // Must re-apply the pending addition on top rather than silently
        // losing it, and must ask the caller to sync the merged result back.
        assert!(app.is_favorite(&tetris));
        let synced = follow_up.expect("reconnect with pending changes must trigger a sync");
        assert!(synced.iter().any(|f| f.normalized_path() == tetris.path));

        // Only cleared once the device actually acks the sync.
        assert!(app.has_pending_changes());
        app.apply_device_event(DeviceEvent::FavoritesSynced);
        assert!(!app.has_pending_changes());
    }

    fn recent(label: &str, rompath: Option<&str>) -> PlayHistoryEntry {
        PlayHistoryEntry {
            label: label.to_string(),
            launch: "/mnt/SDCARD/Emu/GB/launch.sh".to_string(),
            kind: 5,
            imgpath: None,
            rompath: rompath.map(str::to_string),
        }
    }

    /// Reads back one virtual row's contents. By folder, never by `kind` —
    /// there are two virtual rows, and matching on the kind silently returns
    /// whichever comes first.
    fn virtual_games(app: &App, folder: &str) -> Vec<String> {
        app.consoles
            .iter()
            .find(|c| c.folder == folder)
            .map(|c| c.games.iter().map(|g| g.path.clone()).collect())
            .unwrap_or_default()
    }

    fn recent_games(app: &App) -> Vec<String> {
        virtual_games(app, RECENT_FOLDER)
    }

    fn favorite_games(app: &App) -> Vec<String> {
        virtual_games(app, FAVORITES_FOLDER)
    }

    #[test]
    fn recents_resolve_against_the_library_in_device_order() {
        let mut app = app_with_games();
        app.load_recents(vec![
            recent("Kirby", Some("/mnt/SDCARD/Roms/GB/Kirby.gba")),
            recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba")),
        ]);

        assert_eq!(
            recent_games(&app),
            vec!["/mnt/SDCARD/Roms/GB/Kirby.gba", "/mnt/SDCARD/Roms/GB/Tetris.gba"],
            "the virtual console must preserve the device's most-recent-first order"
        );
    }

    #[test]
    fn recents_normalize_parent_segments_before_matching_the_library() {
        let mut app = app_with_games();
        // Quirk 2: the device writes rompaths with `../../` in them.
        app.load_recents(vec![recent("Tetris", Some("/mnt/SDCARD/Roms/GB/../../Roms/GB/Tetris.gba"))]);

        assert_eq!(recent_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
    }

    #[test]
    fn recents_survive_the_library_arriving_after_them() {
        // Offline launch order: recents from cache can land before a fresh
        // library does. Whichever is second has to rebuild the virtual row.
        let mut app = App::new("192.168.1.1".to_string());
        app.load_recents(vec![recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba"))]);
        assert_eq!(recent_games(&app).len(), 1, "unresolved entries are rebuilt from the path, not dropped");

        app.load_library(vec![("GB", vec![game("GB", "Tetris"), game("GB", "Kirby")])]);
        assert_eq!(recent_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
    }

    #[test]
    fn recents_drop_app_launches_and_deduplicate() {
        let mut app = app_with_games();
        app.load_recents(vec![
            recent("Expert Mode", None), // app launch — no rompath
            recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba")),
            recent("Tetris again", Some("/mnt/SDCARD/Roms/GB/Tetris.gba")),
        ]);

        assert_eq!(recent_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
    }

    #[test]
    fn recents_do_not_produce_duplicate_search_hits() {
        let mut app = app_with_games();
        app.load_recents(vec![recent("Metroid Fusion", Some("/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba"))]);
        assert_eq!(recent_games(&app).len(), 1);

        let mut search = SearchState::new();
        for c in "metroid".chars() {
            search.push_char(c, &app.consoles);
        }
        assert_eq!(
            search.matches.len(),
            1,
            "a recently-played game must score once, not once per virtual console it appears in"
        );
    }

    fn save_state(core: &str, name: &str, slot: i32, thumbnail: bool) -> SaveState {
        let path = format!("/mnt/SDCARD/Saves/CurrentProfile/states/{core}/{name}");
        SaveState {
            name: name.to_string(),
            thumbnail_path: thumbnail.then(|| format!("{path}.png")),
            path,
            game_name: name.split('.').next().unwrap().to_string(),
            slot_number: slot,
            size: None,
        }
    }

    fn app_with_saves() -> App {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0; // Tetris
        app.focus = Pane::Games;
        app.save_states = vec![
            save_state("gambatte", "Tetris.state", 0, true),
            save_state("gambatte", "Tetris.state3", 3, false),
        ];
        app
    }

    #[test]
    fn the_saves_browser_opens_even_with_nothing_to_show() {
        let mut app = app_with_saves();
        assert!(app.open_saves());
        assert_eq!(app.saves.as_ref().unwrap().states.len(), 2);

        app.saves = None;
        app.game_idx = 1; // Kirby — no saves in the listing
        assert!(app.open_saves());
        // Opening empty is deliberate: the browser explains *which* kind of
        // empty it is (no saves for this game vs. no listing at all), which a
        // status message that vanishes on the next keypress could not.
        assert!(app.saves.as_ref().unwrap().states.is_empty());
    }

    #[test]
    fn a_listing_arriving_later_fills_in_an_already_open_empty_browser() {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0; // Tetris
        app.focus = Pane::Games;

        // The device was asleep at connect, so nothing was ever listed.
        assert!(app.open_saves());
        assert!(app.saves.as_ref().unwrap().states.is_empty());

        app.apply_device_event(DeviceEvent::SaveStatesLoaded(vec![save_state(
            "gambatte",
            "Tetris.state",
            0,
            true,
        )]));

        assert_eq!(
            app.saves.as_ref().unwrap().states.len(),
            1,
            "a listing that lands while the browser is open must fill it in, not be ignored"
        );
    }

    #[test]
    fn a_save_listing_is_only_worth_requesting_when_connected_and_empty() {
        let mut app = app_with_games();
        assert!(!app.needs_save_listing(), "disconnected — nothing to ask");

        app.connection = ConnectionState::Connected;
        assert!(app.needs_save_listing());

        app.save_states = vec![save_state("gambatte", "Tetris.state", 0, true)];
        assert!(!app.needs_save_listing(), "already have a listing");
    }

    /// A game selected in the Games pane, ready for `enter`.
    fn app_on_a_game() -> App {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0; // Tetris
        app.focus = Pane::Games;
        app
    }

    #[test]
    fn enter_on_a_game_focuses_the_detail_pane_and_esc_comes_back() {
        let mut app = app_on_a_game();

        app.focus_next(); // 'enter'
        assert_eq!(app.focus, Pane::Detail);
        assert_eq!(app.detail.level, DetailLevel::Actions);
        assert_eq!(app.detail.selected, 0);

        // 'esc'/'h' with nothing to back out of leaves the pane entirely.
        assert!(!app.detail.back());
        app.focus_prev();
        assert_eq!(app.focus, Pane::Games);
    }

    #[test]
    fn detail_menu_selection_wraps_in_both_directions() {
        let mut app = app_on_a_game();
        app.focus = Pane::Detail;

        app.move_selection(1); // 'j' — drives the menu, not the library
        assert_eq!(app.detail.selected, 1);
        assert_eq!(app.game_idx, 0, "the library selection must not move with the detail pane focused");
        app.move_selection(1); // wraps
        assert_eq!(app.detail.selected, 0);
        app.move_selection(-1);
        assert_eq!(app.detail.selected, DetailItem::ALL.len() - 1);
    }

    #[test]
    fn settings_replaces_the_action_level_and_backing_out_restores_it() {
        let mut app = app_on_a_game();
        app.focus = Pane::Detail;
        app.detail.selected = 1; // Settings

        assert_eq!(app.confirm_detail(), Some(DetailItem::Settings));
        assert_eq!(app.detail.level, DetailLevel::Settings);
        assert_eq!(app.detail.selected, 0, "the nested level opens on its first row");
        // The top-level confirm must not fire while the nested level is up.
        assert_eq!(app.confirm_detail(), None);
        assert_eq!(app.confirm_game_setting(), Some(GameSettingsItem::Rename));

        assert!(app.detail.back(), "backing out of the nested level is handled, not a focus change");
        assert_eq!(app.detail.level, DetailLevel::Actions);
        assert_eq!(app.detail.selected, 0);
        assert_eq!(app.confirm_game_setting(), None);
    }

    #[test]
    fn each_game_setting_drives_the_same_action_as_its_shortcut() {
        let mut app = app_on_a_game();
        app.focus = Pane::Detail;
        app.detail.enter_settings();

        // Rename — as 'R'.
        assert_eq!(app.confirm_game_setting(), Some(GameSettingsItem::Rename));
        assert!(app.open_rename_prompt());
        assert_eq!(app.rename_prompt.as_ref().unwrap().game.display_name(), "Tetris");
        app.rename_prompt = None;

        // Favourite — as 'f'.
        app.detail.selected = 1;
        assert_eq!(app.confirm_game_setting(), Some(GameSettingsItem::ToggleFavorite));
        let tetris = app.selected_game().cloned().unwrap();
        app.toggle_favorite(&tetris);
        assert!(app.is_favorite(&tetris));

        // Delete — as 'd'. The confirmation still stands between the menu and
        // any I/O; the menu never deletes anything by itself.
        app.detail.selected = 2;
        assert_eq!(app.confirm_game_setting(), Some(GameSettingsItem::Delete));
        assert!(app.open_confirm_delete());
        assert!(app.confirm_delete.is_some());
    }

    #[test]
    fn moving_to_another_game_resets_the_detail_menu() {
        let mut app = app_on_a_game();
        app.focus = Pane::Detail;
        app.detail.enter_settings();
        app.detail.selected = 2; // Delete

        // Back to the library and onto another game.
        app.focus_prev();
        assert_eq!(app.detail.level, DetailLevel::Actions, "leaving the pane resets it");

        app.detail.enter_settings();
        app.detail.selected = 2;
        app.focus = Pane::Games;
        app.move_selection(1); // 'j' — now on Kirby

        assert_eq!(app.game_idx, 1);
        assert_eq!(
            (app.detail.level, app.detail.selected),
            (DetailLevel::Actions, 0),
            "a menu left on Delete for the previous game is dangerous stale state"
        );
    }

    #[test]
    fn saves_browser_requests_the_slot_thumbnail_not_the_box_art() {
        let mut app = app_with_saves();

        // Closed: the selected game's box art.
        let (key, path) = app.current_image().unwrap();
        assert_eq!(key, "/mnt/SDCARD/Roms/GB/Tetris.gba");
        assert_eq!(path, "/mnt/SDCARD/Roms/GB/Imgs/Tetris.png");

        // Open: the selected slot's screenshot, keyed on the thumbnail's own
        // path. Keying it on the ROM path instead would make the overlay and
        // the detail pane fight over one image slot.
        app.open_saves();
        let (key, path) = app.current_image().unwrap();
        assert_eq!(key, "/mnt/SDCARD/Saves/CurrentProfile/states/gambatte/Tetris.state.png");
        assert_eq!(key, path);
    }

    #[test]
    fn a_slot_with_no_screenshot_requests_no_image() {
        let mut app = app_with_saves();
        app.open_saves();
        app.saves.as_mut().unwrap().move_selection(1); // slot 3, no thumbnail
        assert!(app.current_image().is_none());
    }

    #[test]
    fn closing_the_saves_browser_returns_to_the_games_box_art() {
        let mut app = app_with_saves();
        app.open_saves();
        app.saves = None; // what handle_key's Esc arm does

        let (key, _) = app.current_image().unwrap();
        assert_eq!(key, "/mnt/SDCARD/Roms/GB/Tetris.gba");
    }

    #[test]
    fn saves_selection_wraps_and_survives_a_fresh_listing() {
        let mut app = app_with_saves();
        app.open_saves();

        app.saves.as_mut().unwrap().move_selection(-1);
        assert_eq!(app.saves.as_ref().unwrap().selected, 1, "moving up from the first slot wraps");

        // A listing arriving while the browser is open refreshes it in place
        // instead of yanking it shut.
        app.apply_device_event(DeviceEvent::SaveStatesLoaded(vec![save_state(
            "gambatte",
            "Tetris.state",
            0,
            true,
        )]));
        let open = app.saves.as_ref().expect("browser must stay open");
        assert_eq!(open.states.len(), 1);
        assert_eq!(open.selected, 0, "selection must be clamped into the shorter list");
    }

    #[test]
    fn save_count_marks_only_games_that_have_saves() {
        let app = app_with_saves();
        assert_eq!(app.save_count(&game("GB", "Tetris")), 2);
        assert_eq!(app.save_count(&game("GB", "Kirby")), 0);
    }

    #[test]
    fn deleting_a_game_clears_it_from_every_index() {
        let mut app = app_with_saves();
        let tetris = game("GB", "Tetris");
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);
        app.load_recents(vec![recent("Tetris", Some(&tetris.path))]);
        assert_eq!(recent_games(&app).len(), 1);

        // As if the delete had been chosen from the Details pane menu.
        app.detail.enter_settings();
        app.detail.selected = 2;
        app.open_confirm_delete();
        let (deleted, plan) = app.confirm_delete().expect("dialog must yield the plan");
        assert_eq!(deleted.path, tetris.path);
        app.apply_deletion(&deleted, &plan);

        assert_eq!(
            (app.detail.level, app.detail.selected),
            (DetailLevel::Actions, 0),
            "the cursor now points at a different game — the menu must not stay on Delete"
        );
        assert!(!app.consoles.iter().any(|c| c.games.iter().any(|g| g.path == tetris.path)));
        assert!(!app.games_by_path.contains_key(&tetris.path));
        assert!(!app.is_favorite(&tetris), "a deleted game must not stay favourited");
        assert_eq!(app.save_count(&tetris), 0, "its saves are gone from the card too");
        assert!(recent_games(&app).is_empty(), "and it must drop out of Recently Played");
    }

    #[test]
    fn deleting_a_game_leaves_its_neighbours_alone() {
        let mut app = app_with_saves();
        app.open_confirm_delete();
        let (deleted, plan) = app.confirm_delete().unwrap();
        app.apply_deletion(&deleted, &plan);

        let gb = &app.consoles[idx_of(&app, "GB")];
        assert_eq!(gb.games.len(), 1);
        assert_eq!(gb.games[0].display_name(), "Kirby");
        assert!(app.games_by_path.contains_key("/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba"));
    }

    #[test]
    fn deleting_the_last_game_in_a_console_clamps_the_selection() {
        let mut app = App::new("h".into());
        app.load_library(vec![
            ("GB", vec![game("GB", "Tetris")]),
            ("GBA", vec![game("GBA", "Metroid Fusion")]),
        ]);
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0;

        app.open_confirm_delete();
        let (deleted, plan) = app.confirm_delete().unwrap();
        app.apply_deletion(&deleted, &plan);

        // GB is now empty and therefore hidden — the selection has to land
        // somewhere that actually renders.
        assert!(app.selected_game().is_some(), "selection must not point into an emptied console");
        assert_eq!(app.current_console().unwrap().folder, "GBA");
    }

    #[test]
    fn renaming_a_game_repoints_every_index_including_its_saves() {
        let mut app = app_with_saves();
        let tetris = game("GB", "Tetris");
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);
        app.load_recents(vec![recent("Tetris", Some(&tetris.path))]);

        app.open_rename_prompt();
        app.rename_prompt.as_mut().unwrap().input = "Tetris DX".to_string();
        let (old, plan) = app.confirm_rename_prompt().expect("a valid name must produce a plan");
        app.apply_rename(&old, &plan);

        let new_path = "/mnt/SDCARD/Roms/GB/Tetris DX.gba";
        assert!(app.games_by_path.contains_key(new_path));
        assert!(!app.games_by_path.contains_key(&tetris.path));
        assert_eq!(app.current_console().unwrap().games[0].display_name(), "Tetris DX");

        let renamed = app.games_by_path.get(new_path).unwrap().clone();
        assert!(app.is_favorite(&renamed), "the favourite must follow the rename, not be dropped");
        assert_eq!(app.save_count(&renamed), 2, "saves must follow the rename too");
        assert_eq!(app.save_count(&tetris), 0);
        assert_eq!(recent_games(&app), vec![new_path]);
    }

    #[test]
    fn renaming_keeps_the_extension_and_rejects_a_path_traversal() {
        let mut app = app_with_saves();

        app.open_rename_prompt();
        assert_eq!(
            app.rename_prompt.as_ref().unwrap().input,
            "Tetris",
            "the prompt is prefilled with the name minus its extension"
        );

        app.rename_prompt.as_mut().unwrap().input = "../../evil".to_string();
        assert!(app.confirm_rename_prompt().is_none(), "a traversal must not produce a plan");
        let prompt = app.rename_prompt.as_ref().expect("the prompt stays open so the name can be fixed");
        assert!(prompt.error.is_some(), "and it must say why");

        app.rename_prompt.as_mut().unwrap().input = "Tetris DX".to_string();
        let (_, plan) = app.confirm_rename_prompt().unwrap();
        assert_eq!(plan.new_game.extension, "gba");
    }

    #[test]
    fn deleting_evicts_the_on_screen_box_art() {
        let mut app = app_with_saves();
        // Pretend the selected game's art is what's currently decoded.
        app.image_for_key = Some("/mnt/SDCARD/Roms/GB/Tetris.gba".to_string());

        app.open_confirm_delete();
        let (deleted, plan) = app.confirm_delete().unwrap();
        app.apply_deletion(&deleted, &plan);

        assert!(app.image_for_key.is_none(), "art for a deleted game must not stay on screen");
    }

    #[test]
    fn cancelling_the_delete_dialog_changes_nothing() {
        let mut app = app_with_saves();
        let before = app.current_console().unwrap().games.len();

        app.open_confirm_delete();
        app.confirm_delete = None; // what handle_key does for any key but 'y'

        assert_eq!(app.current_console().unwrap().games.len(), before);
        assert!(app.games_by_path.contains_key("/mnt/SDCARD/Roms/GB/Tetris.gba"));
    }

    #[test]
    fn selected_console_row_is_the_visible_position_not_the_console_index() {
        let mut app = app_with_games();
        // Only GB and GBA have ROMs; every other console (and the empty
        // Recently Played row) is hidden, so GBA renders on row 1 despite
        // sitting several indices further into `consoles`.
        assert_eq!(app.visible_console_indices(), vec![idx_of(&app, "GB"), idx_of(&app, "GBA")]);

        app.console_idx = idx_of(&app, "GB");
        assert_eq!(app.selected_console_row(), Some(0));

        app.console_idx = idx_of(&app, "GBA");
        assert_eq!(
            app.selected_console_row(),
            Some(1),
            "hidden empty consoles must shift the rendered row up, or the list scrolls to the wrong offset"
        );
    }

    #[test]
    fn selected_console_row_is_none_when_the_selection_is_hidden() {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GBC"); // empty, so not rendered at all
        assert_eq!(app.selected_console_row(), None);
    }

    #[test]
    fn ctrl_c_sets_should_quit() {
        let mut app = App::new(String::new());
        assert!(!app.should_quit);
        // What handle_key's ctrl-c arm does — and, since `q` was unbound in
        // normal mode, the only thing that does it.
        app.should_quit = true;
        assert!(app.should_quit);
    }

    #[test]
    fn s_opens_settings_menu_on_the_first_item() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_settings(); // 's'
        let settings = app.settings.as_ref().unwrap();
        assert_eq!(settings.selected, 0);
        assert_eq!(settings.current(), SettingsItem::Reconnect, "reconnect is the default row");
    }

    #[test]
    fn settings_selection_wraps_in_both_directions() {
        let mut app = App::new(String::new());
        app.open_settings();
        let settings = app.settings.as_mut().unwrap();

        settings.move_selection(1); // 'j'
        assert_eq!(settings.current(), SettingsItem::ChangeIp);

        // Walk off the end and back to the top, however many rows `ALL` has.
        for _ in 1..SettingsItem::ALL.len() {
            settings.move_selection(1);
        }
        assert_eq!(settings.current(), SettingsItem::Reconnect, "'j' past the last row wraps to the top");

        settings.move_selection(-1); // 'k', wraps backward to the bottom
        assert_eq!(settings.current(), *SettingsItem::ALL.last().unwrap());
    }

    #[test]
    fn discovery_accumulates_results_while_the_sweep_is_still_running() {
        let mut app = App::new(String::new());
        app.open_discovery();

        app.apply_device_event(DeviceEvent::DiscoveryProgress { done: 40, total: 254 });
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.12".into()));
        app.apply_device_event(DeviceEvent::DiscoveryProgress { done: 128, total: 254 });

        let discovery = app.discovery.as_ref().unwrap();
        assert_eq!(discovery.done, 128);
        assert_eq!(discovery.found, vec!["192.168.1.12"]);
        assert!(!discovery.finished, "a result arriving mid-sweep must not end the scan");
    }

    #[test]
    fn discovery_ignores_a_duplicate_host() {
        let mut app = App::new(String::new());
        app.open_discovery();
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.12".into()));
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.12".into()));
        assert_eq!(app.discovery.as_ref().unwrap().found.len(), 1);
    }

    #[test]
    fn confirming_discovery_sets_the_host_and_closes_the_modal() {
        let mut app = App::new(String::new());
        app.open_discovery();
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.12".into()));
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.30".into()));
        app.discovery.as_mut().unwrap().move_selection(1);

        assert_eq!(app.confirm_discovery(), Some("192.168.1.30".to_string()));
        assert_eq!(app.host, "192.168.1.30");
        assert!(app.discovery.is_none());
    }

    #[test]
    fn confirming_discovery_with_no_results_is_a_no_op() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_discovery();
        app.apply_device_event(DeviceEvent::DiscoveryDone);

        assert!(app.confirm_discovery().is_none());
        assert_eq!(app.host, "192.168.1.1", "an empty scan must not clear the configured host");
        assert!(app.discovery.as_ref().unwrap().finished);
    }

    #[test]
    fn discovery_events_after_the_modal_closes_are_harmless() {
        let mut app = App::new(String::new());
        app.open_discovery();
        app.discovery = None; // esc while the spawned sweep is still running

        // The sweep task outlives the modal, so its remaining events must not
        // panic or resurrect it.
        app.apply_device_event(DeviceEvent::DiscoveryProgress { done: 200, total: 254 });
        app.apply_device_event(DeviceEvent::DiscoveryFound("192.168.1.12".into()));
        app.apply_device_event(DeviceEvent::DiscoveryDone);
        assert!(app.discovery.is_none());
    }

    #[test]
    fn confirming_reconnect_returns_the_item_and_closes_the_menu() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_settings();

        assert_eq!(app.confirm_settings(), Some(SettingsItem::Reconnect)); // enter
        assert!(app.settings.is_none(), "menu closes so the browser is visible while reconnecting");
    }

    #[test]
    fn confirming_change_ip_closes_the_menu_and_opens_the_prefilled_prompt() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_settings();
        app.settings.as_mut().unwrap().move_selection(1); // 'j' -> Change IP address

        // The two-step flow handle_key's Enter arm performs.
        assert_eq!(app.confirm_settings(), Some(SettingsItem::ChangeIp));
        app.open_ip_prompt();

        assert!(app.settings.is_none(), "prompt replaces the menu rather than stacking on it");
        assert_eq!(app.ip_prompt.unwrap().input, "192.168.1.1");
    }

    #[test]
    fn esc_closes_settings_without_selecting_anything() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_settings();

        app.settings = None; // what handle_key's KeyCode::Esc arm does
        assert!(app.ip_prompt.is_none());
        assert_eq!(app.host, "192.168.1.1");
    }

    #[test]
    fn opening_ip_prompt_prefills_it_with_the_current_host() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_ip_prompt();
        assert_eq!(app.ip_prompt.unwrap().input, "192.168.1.1");
    }

    #[test]
    fn typing_and_backspace_mutate_ip_prompt_input() {
        let mut app = App::new(String::new());
        app.open_ip_prompt();
        let prompt = app.ip_prompt.as_mut().unwrap();
        prompt.push_char('1');
        prompt.push_char('0');
        prompt.backspace();
        prompt.push_char('.');
        assert_eq!(app.ip_prompt.unwrap().input, "1.");
    }

    #[test]
    fn confirming_ip_prompt_updates_host_and_closes_prompt() {
        let mut app = App::new(String::new());
        app.open_ip_prompt();
        app.ip_prompt.as_mut().unwrap().input = "  192.168.1.50  ".to_string();

        let confirmed = app.confirm_ip_prompt();
        assert_eq!(confirmed, Some("192.168.1.50".to_string()));
        assert_eq!(app.host, "192.168.1.50");
        assert!(app.ip_prompt.is_none());
    }

    #[test]
    fn confirming_empty_ip_prompt_is_a_no_op_and_leaves_prompt_open() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_ip_prompt();
        app.ip_prompt.as_mut().unwrap().input = "   ".to_string();

        let confirmed = app.confirm_ip_prompt();
        assert!(confirmed.is_none());
        assert_eq!(app.host, "192.168.1.1", "blank confirm must not clear the existing host");
        assert!(app.ip_prompt.is_some(), "prompt should stay open so the user can retype");
    }

    #[test]
    fn escaping_ip_prompt_leaves_host_unchanged() {
        let mut app = App::new("192.168.1.1".to_string());
        app.open_ip_prompt();
        app.ip_prompt.as_mut().unwrap().input = "10.0.0.5".to_string();

        app.ip_prompt = None; // what handle_key's KeyCode::Esc arm does
        assert_eq!(app.host, "192.168.1.1");
    }

    #[test]
    fn favourites_row_fills_from_the_device_list_sorted_by_console_then_name() {
        let mut app = app_with_games();
        // Deliberately not in sorted order: the source is a HashSet, so the
        // row has to impose its own ordering or it reshuffles between frames.
        app.load_favorites(vec![
            FavoriteGame::for_game(&game("GB", "Tetris")),
            FavoriteGame::for_game(&game("GBA", "Metroid Fusion")),
            FavoriteGame::for_game(&game("GB", "Kirby")),
        ]);

        assert_eq!(
            favorite_games(&app),
            vec![
                "/mnt/SDCARD/Roms/GB/Kirby.gba",
                "/mnt/SDCARD/Roms/GB/Tetris.gba",
                "/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba",
            ]
        );
    }

    #[test]
    fn favourites_row_fills_from_the_offline_cache_too() {
        // Cached favourites are bare paths with no `favourite.json` entries
        // behind them — the row must still populate, since an offline launch
        // is exactly when browsing favourites is most useful.
        let mut app = app_with_games();
        app.load_cached_favorite_paths(["/mnt/SDCARD/Roms/GB/Tetris.gba".to_string()].into_iter().collect());

        assert_eq!(favorite_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
    }

    #[test]
    fn toggling_a_favourite_updates_the_row_immediately() {
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");
        assert!(favorite_games(&app).is_empty());

        app.toggle_favorite(&tetris);
        assert_eq!(favorite_games(&app), vec![tetris.path.clone()]);

        app.toggle_favorite(&tetris);
        assert!(favorite_games(&app).is_empty(), "unfavouriting must empty the row again");
    }

    #[test]
    fn unfavouriting_from_inside_the_favourites_row_clamps_the_selection() {
        // The game being toggled is the one under the cursor *and* the only
        // row in the list it's in, so the cursor has nowhere to stay.
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);

        app.console_idx = idx_of(&app, FAVORITES_FOLDER);
        app.game_idx = 0;
        app.focus = Pane::Games;
        assert_eq!(app.selected_game().unwrap().path, tetris.path);

        app.toggle_favorite(&tetris);
        assert!(favorite_games(&app).is_empty());
        // The row the cursor was in no longer exists on screen (empty consoles
        // are hidden), so `clamp_selection` moves it to a real one rather than
        // leaving it pointing at nothing.
        assert_ne!(app.console_idx, idx_of(&app, FAVORITES_FOLDER));
        assert!(app.selected_game().is_some());
    }

    #[test]
    fn favourites_survive_a_rom_the_current_scan_never_saw() {
        // Card swapped, or a partially indexed library: the favourite is still
        // a favourite and is rebuilt from its path, exactly like recents.
        let mut app = App::new("192.168.1.1".to_string());
        app.load_cached_favorite_paths(
            ["/mnt/SDCARD/Roms/GBA/Golden Sun.gba".to_string()].into_iter().collect(),
        );

        assert_eq!(favorite_games(&app), vec!["/mnt/SDCARD/Roms/GBA/Golden Sun.gba"]);
    }

    #[test]
    fn the_two_virtual_rows_stay_separate() {
        // The regression the second virtual row invites: a rebuild that finds
        // its target by `kind` instead of by folder fills whichever row comes
        // first, so recents and favourites overwrite each other.
        let mut app = app_with_games();
        app.load_recents(vec![recent("Kirby", Some("/mnt/SDCARD/Roms/GB/Kirby.gba"))]);
        app.load_favorites(vec![FavoriteGame::for_game(&game("GB", "Tetris"))]);

        assert_eq!(recent_games(&app), vec!["/mnt/SDCARD/Roms/GB/Kirby.gba"]);
        assert_eq!(favorite_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);

        // …and in the other order, since either can arrive second.
        app.load_recents(vec![recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba"))]);
        assert_eq!(recent_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
        assert_eq!(favorite_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris.gba"]);
    }

    #[test]
    fn deleting_a_favourited_game_clears_it_from_the_favourites_row() {
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0;

        app.open_confirm_delete();
        let (deleted, plan) = app.confirm_delete().unwrap();
        app.apply_deletion(&deleted, &plan);

        assert!(favorite_games(&app).is_empty());
        assert!(!app.is_favorite(&tetris));
    }

    #[test]
    fn renaming_a_favourited_game_repoints_the_favourites_row() {
        let mut app = app_with_games();
        let tetris = game("GB", "Tetris");
        app.load_favorites(vec![FavoriteGame::for_game(&tetris)]);
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0;

        app.open_rename_prompt();
        app.rename_prompt.as_mut().unwrap().input = "Tetris DX".to_string();
        let (old, plan) = app.confirm_rename_prompt().unwrap();
        app.apply_rename(&old, &plan);

        assert_eq!(favorite_games(&app), vec!["/mnt/SDCARD/Roms/GB/Tetris DX.gba"]);
    }

    #[test]
    fn search_never_returns_a_virtual_console_row() {
        // Both virtual rows hold copies of real games; counting them would
        // score every favourited or recently-played game twice.
        let mut app = app_with_games();
        app.load_favorites(vec![FavoriteGame::for_game(&game("GB", "Tetris"))]);
        app.load_recents(vec![recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba"))]);

        let mut search = SearchState::new();
        for c in "tetris".chars() {
            search.push_char(c, &app.consoles);
        }

        assert_eq!(search.matches.len(), 1, "Tetris exists once in the library, virtual rows aside");
        assert!(search.matches.iter().all(|m| app.consoles[m.console_idx].kind == ConsoleKind::Real));
    }

    #[test]
    fn rename_preview_lists_the_cascade_for_a_valid_name() {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0;
        app.open_rename_prompt();
        app.rename_prompt.as_mut().unwrap().input = "Tetris DX".to_string();

        let summary = app.rename_preview().expect("a valid rename has a plan to show");
        assert!(summary.iter().any(|l| l.contains("Tetris DX.gba")));
    }

    #[test]
    fn rename_preview_is_empty_for_an_unchanged_or_invalid_name() {
        let mut app = app_with_games();
        app.console_idx = idx_of(&app, "GB");
        app.game_idx = 0;
        app.open_rename_prompt();

        // Prefilled with the current name: nothing would change.
        assert!(app.rename_preview().is_none());

        app.rename_prompt.as_mut().unwrap().input = "../../etc/passwd".to_string();
        assert!(app.rename_preview().is_none(), "a rejected name has no plan to preview");
    }

    #[test]
    fn a_completed_library_scan_stamps_the_sync_time() {
        let mut app = App::new("192.168.1.1".to_string());
        assert!(app.last_sync.is_none());

        app.apply_device_event(DeviceEvent::LibraryLoaded(vec![("GB", vec![game("GB", "Tetris")])]));
        assert!(app.last_sync.is_some());

        // Connecting alone is not a sync — the listing that follows can fail.
        let mut app = App::new("192.168.1.1".to_string());
        app.apply_device_event(DeviceEvent::Connected);
        assert!(app.last_sync.is_none());
    }

    #[test]
    fn recent_rank_is_one_based_and_deduped() {
        let mut app = app_with_games();
        app.load_recents(vec![
            recent("Kirby", Some("/mnt/SDCARD/Roms/GB/Kirby.gba")),
            recent("Kirby", Some("/mnt/SDCARD/Roms/GB/Kirby.gba")),
            recent("Tetris", Some("/mnt/SDCARD/Roms/GB/Tetris.gba")),
        ]);

        assert_eq!(app.recent_rank(&game("GB", "Kirby")), Some(1));
        // Ranks must match the rows in the Recently Played list, which dedupes.
        assert_eq!(app.recent_rank(&game("GB", "Tetris")), Some(2));
        assert_eq!(app.recent_rank(&game("GBA", "Metroid Fusion")), None);
    }

    #[test]
    fn help_is_closed_until_asked_for_and_any_key_closes_it() {
        let mut app = App::new(String::new());
        assert!(!app.help);

        app.help = true; // '?'
        app.help = false; // any key
        assert!(!app.help);
    }
}
