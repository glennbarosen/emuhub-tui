//! Core, headless device-protocol logic for EmuHub TUI — models, path
//! quirks, NDJSON favourites, library scanning, XDG cache, and the
//! russh-based SSH/SFTP transport. No TUI dependencies live here, so this
//! crate stays usable from a future headless CLI (`emuhub ls GBA`,
//! `emuhub sync`) and is fully unit-testable without hardware.
//!
//! See `docs/DEVICE-PROTOCOL.md` for the device-protocol knowledge this crate
//! encodes — where Onion OS keeps things, and the file-format quirks that make
//! writing to the card harder than it looks.

pub mod cache;
pub mod cascade;
pub mod consoles;
pub mod discover;
pub mod error;
pub mod favorites;
pub mod import;
pub mod models;
pub mod path;
pub mod saves;
pub mod scan;
pub mod transport;

pub use error::{Error, Result};
