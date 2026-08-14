//! `/` fuzzy-searches every console at once — something the SwiftUI original
//! cannot do. Backed by `nucleo-matcher`, the same matcher fzf/helix/nushell
//! use.

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};

use crate::app::{ConsoleEntry, ConsoleKind};

pub struct SearchMatch {
    pub console_idx: usize,
    pub game_idx: usize,
    pub score: u32,
}

pub struct SearchState {
    pub query: String,
    pub matches: Vec<SearchMatch>,
    pub selected: usize,
}

impl SearchState {
    pub fn new() -> Self {
        Self { query: String::new(), matches: Vec::new(), selected: 0 }
    }

    pub fn push_char(&mut self, c: char, consoles: &[ConsoleEntry]) {
        self.query.push(c);
        self.recompute(consoles);
    }

    pub fn backspace(&mut self, consoles: &[ConsoleEntry]) {
        self.query.pop();
        self.recompute(consoles);
    }

    /// Re-runs the fuzzy match across every console's games. O(n) over the
    /// whole library per keystroke — fine at handheld-library scale (a few
    /// thousand ROMs, not a few million).
    pub fn recompute(&mut self, consoles: &[ConsoleEntry]) {
        self.selected = 0;
        self.matches.clear();
        if self.query.is_empty() {
            return;
        }

        let mut matcher = Matcher::new(nucleo::Config::DEFAULT);
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);

        let mut scored: Vec<SearchMatch> = Vec::new();
        for (console_idx, console) in consoles.iter().enumerate() {
            // Virtual consoles (Recently Played) are views over games that
            // already live under a real console — scoring them too would show
            // every recently-played game twice in the results.
            if console.kind == ConsoleKind::Virtual {
                continue;
            }
            for (game_idx, game) in console.games.iter().enumerate() {
                let mut buf = Vec::new();
                let haystack = Utf32Str::new(game.display_name(), &mut buf);
                if let Some(score) = pattern.score(haystack, &mut matcher) {
                    scored.push(SearchMatch { console_idx, game_idx, score });
                }
            }
        }
        scored.sort_by_key(|m| std::cmp::Reverse(m.score));
        self.matches = scored;
    }
}

impl Default for SearchState {
    fn default() -> Self {
        Self::new()
    }
}
