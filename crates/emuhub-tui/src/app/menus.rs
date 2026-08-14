//! Overlay and menu state: everything the browser can show on top of itself
//! (the IP prompt, settings popup, discovery modal, rename prompt, saves
//! browser, delete confirmation) plus the Details pane's own menu machinery.
//! Split out of `app.rs` because it's the part of `App`'s state that grows by
//! one self-contained struct per new modal, rather than by touching existing
//! logic — see `AGENTS.md`'s note on the `Option<State>`-per-modal pattern.

use emuhub_core::cascade::DeletePlan;
use emuhub_core::models::{GameFile, SaveState};
use ratatui::widgets::ListState;

/// A single-line text input for entering the device's IP/hostname — the
/// same push_char/backspace shape as `SearchState`, minus the recompute step
/// (there's nothing to match against as the user types).
pub struct IpPromptState {
    pub input: String,
}

impl IpPromptState {
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
    }

    pub fn backspace(&mut self) {
        self.input.pop();
    }
}

/// One row in the settings popup. Adding a setting later is one variant, one
/// entry in `ALL`, and one arm in `main.rs`'s dispatch — the menu itself is
/// driven entirely off `ALL` and never needs touching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsItem {
    Reconnect,
    ChangeIp,
    FindDevice,
}

impl SettingsItem {
    /// Reconnect first — the more frequent action, so it's the row the menu
    /// opens on and `s` + `enter` is a two-keystroke reconnect.
    pub const ALL: &'static [SettingsItem] =
        &[SettingsItem::Reconnect, SettingsItem::ChangeIp, SettingsItem::FindDevice];

    pub fn label(self) -> &'static str {
        match self {
            SettingsItem::Reconnect => "Reconnect",
            SettingsItem::ChangeIp => "Change IP address",
            SettingsItem::FindDevice => "Find device on network",
        }
    }
}

/// The `?` overlay's content, as `(section, [(keys, meaning)])`. Data rather
/// than a hand-laid-out string so `ui::draw_help` can align the key column and
/// a new binding is one tuple — the same "menus are driven off a table"
/// approach as `SettingsItem::ALL`.
pub const HELP_SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "NAVIGATE",
        &[
            ("j / k  ↑ / ↓", "move"),
            ("h / l  ← / →", "back / forward a pane"),
            ("enter", "into a pane · run the selected action"),
            ("esc", "back out one level"),
            ("g / G", "jump to first / last"),
        ],
    ),
    (
        "LIBRARY",
        &[
            ("/", "fuzzy search every console at once"),
            ("r", "refresh the library from the device"),
            ("s", "settings — reconnect, change IP, find device"),
            ("?", "this help"),
            ("ctrl-c", "quit"),
        ],
    ),
    ("DIALOGS", &[("q / esc", "close an overlay"), ("y", "confirm a delete — any other key cancels")]),
];

/// The line under the help table. The absence of per-game shortcuts is a
/// deliberate design decision (one route to each action, not a menu plus
/// hidden keys to keep in sync), so the help has to say so — otherwise it
/// reads as an omission.
pub const HELP_FOOTNOTE: &str =
    "Per-game actions — saves, favourite, rename, delete — have no shortcuts on purpose. \
     Press enter on a game to reach them in the Details pane.";

/// Which level of the Details pane's action list is showing. The nested level
/// *replaces* the top one rather than stacking, matching how
/// `SettingsItem::ChangeIp` opens the IP prompt — backing out returns to the
/// browser, not to a half-remembered stack of menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailLevel {
    Actions,
    Settings,
}

/// Top level of the Details pane menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailItem {
    ShowSaves,
    Settings,
}

impl DetailItem {
    pub const ALL: &'static [DetailItem] = &[DetailItem::ShowSaves, DetailItem::Settings];

    pub fn label(self) -> &'static str {
        match self {
            DetailItem::ShowSaves => "Show saves",
            DetailItem::Settings => "Settings",
        }
    }
}

/// The per-game settings level: rename, favourite, delete. This menu is the
/// *only* route to them — there are deliberately no key bindings, so there's
/// one place to find each action rather than a menu plus hidden shortcuts that
/// drift out of sync with it. `HELP_FOOTNOTE` says as much to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameSettingsItem {
    Rename,
    ToggleFavorite,
    Delete,
}

impl GameSettingsItem {
    /// Delete last, and deliberately not the row the level opens on.
    pub const ALL: &'static [GameSettingsItem] =
        &[GameSettingsItem::Rename, GameSettingsItem::ToggleFavorite, GameSettingsItem::Delete];

    /// The favourite row is the one label that depends on current state, so it
    /// takes the game's status rather than being a bare `&'static str`.
    pub fn label(self, is_favorite: bool) -> &'static str {
        match self {
            GameSettingsItem::Rename => "Rename",
            GameSettingsItem::ToggleFavorite => {
                if is_favorite {
                    "Unfavourite"
                } else {
                    "Favourite"
                }
            }
            GameSettingsItem::Delete => "Delete",
        }
    }
}

/// The Details pane's menu cursor. Unlike every other menu in the app this
/// isn't an `Option<...>` overlay: the pane is always drawn, so the list is
/// always rendered (dimmed) and only becomes live when `focus` is
/// `Pane::Detail`.
pub struct DetailState {
    pub level: DetailLevel,
    pub selected: usize,
}

impl DetailState {
    pub fn new() -> Self {
        Self { level: DetailLevel::Actions, selected: 0 }
    }

    pub fn len(&self) -> usize {
        match self.level {
            DetailLevel::Actions => DetailItem::ALL.len(),
            DetailLevel::Settings => GameSettingsItem::ALL.len(),
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }

    /// Enters the nested level, from its first row.
    pub fn enter_settings(&mut self) {
        self.level = DetailLevel::Settings;
        self.selected = 0;
    }

    /// Backs out of the nested level. Returns `false` if there was nothing to
    /// back out of, so the caller knows to move focus instead.
    pub fn back(&mut self) -> bool {
        match self.level {
            DetailLevel::Settings => {
                *self = Self::new();
                true
            }
            DetailLevel::Actions => false,
        }
    }
}

impl Default for DetailState {
    fn default() -> Self {
        Self::new()
    }
}

/// The save-state browser overlay for one game. Holds its own resolved list
/// rather than re-deriving it per frame, so the selection stays meaningful
/// even if a fresh listing arrives from the device while it's open.
pub struct SavesState {
    pub game: GameFile,
    pub states: Vec<SaveState>,
    pub selected: usize,
    pub list: ListState,
}

impl SavesState {
    pub fn move_selection(&mut self, delta: isize) {
        if self.states.is_empty() {
            return;
        }
        let len = self.states.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }

    pub fn current(&self) -> Option<&SaveState> {
        self.states.get(self.selected)
    }
}

/// The delete confirmation. Holds the whole plan, not just the game: the
/// dialog's job is to show the user every file that is about to disappear
/// *before* they commit, which is only possible because the cascade is
/// computed as data first (see `emuhub_core::cascade`).
pub struct ConfirmDeleteState {
    pub game: GameFile,
    pub plan: DeletePlan,
}

/// The rename prompt — same single-line-input shape as `IpPromptState`,
/// prefilled with the current name so it's an edit, not a retype.
pub struct RenamePromptState {
    pub game: GameFile,
    pub input: String,
    /// Set when the typed name is rejected, so the dialog can say why instead
    /// of silently refusing to accept `enter`.
    pub error: Option<String>,
}

impl RenamePromptState {
    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.error = None;
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.error = None;
    }
}

/// The device-discovery modal: a progress readout while the sweep runs, and a
/// pick list of whatever it confirmed. Results are selectable as they arrive,
/// so a device found at `.12` can be chosen without waiting for `.254`.
pub struct DiscoveryState {
    pub done: usize,
    pub total: usize,
    pub finished: bool,
    pub found: Vec<String>,
    /// The range being swept (`192.168.68.0/22`). Shown while scanning
    /// because an empty result is ambiguous otherwise: a wrong-looking range
    /// here is the difference between "the handheld is off" and "the scan
    /// never looked at the handheld's half of the network".
    pub network: Option<String>,
    /// Hosts that answered on the SSH port but failed the identity check,
    /// with the reason. Surfaced rather than dropped so a device that refuses
    /// our credentials doesn't read as an empty network.
    pub rejected: Vec<(String, String)>,
    pub selected: usize,
    pub list: ListState,
}

impl DiscoveryState {
    pub fn new() -> Self {
        Self {
            done: 0,
            total: 0,
            finished: false,
            found: Vec::new(),
            network: None,
            rejected: Vec::new(),
            selected: 0,
            list: ListState::default(),
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.found.is_empty() {
            return;
        }
        let len = self.found.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(len)) as usize;
    }

    pub fn current(&self) -> Option<&String> {
        self.found.get(self.selected)
    }
}

impl Default for DiscoveryState {
    fn default() -> Self {
        Self::new()
    }
}

/// The settings popup's selection. Same wrapping navigation as the console
/// and game lists, so the menu is driven with the keys already in the hands.
pub struct SettingsState {
    pub selected: usize,
}

impl SettingsState {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = SettingsItem::ALL.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    pub fn current(&self) -> SettingsItem {
        SettingsItem::ALL[self.selected]
    }
}

impl Default for SettingsState {
    fn default() -> Self {
        Self::new()
    }
}
