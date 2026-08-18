//! Static device-protocol knowledge: console list, ROM extensions, and the
//! console→RetroArch-core mapping used to locate save states (Quirk 3).
//! Ported verbatim from the Swift original's `Console.allSystems` and
//! `FileSystemService.consoleCoreNames` — this is the hardest-won knowledge in
//! the source repo, so it is not re-derived, only copied. Written up in
//! `docs/DEVICE-PROTOCOL.md` §5 and §7.

use crate::models::Console;

pub const ALL_SYSTEMS: &[Console] = &[
    Console { name: "Game Boy", folder: "GB", icon: "🎮" },
    Console { name: "Game Boy Color", folder: "GBC", icon: "🎮" },
    Console { name: "Game Boy Advance", folder: "GBA", icon: "🎮" },
    Console { name: "NES / Famicom", folder: "FC", icon: "🕹️" },
    Console { name: "Super Nintendo", folder: "SFC", icon: "🎯" },
    Console { name: "Sega Genesis", folder: "MD", icon: "🎮" },
    Console { name: "Sega Master System", folder: "MS", icon: "🎮" },
    Console { name: "PlayStation", folder: "PS", icon: "💿" },
    Console { name: "Arcade (MAME)", folder: "ARCADE", icon: "👾" },
    Console { name: "Atari 2600", folder: "ATARI", icon: "🕹️" },
    Console { name: "Atari Lynx", folder: "LYNX", icon: "🎮" },
    Console { name: "Neo Geo", folder: "NEOGEO", icon: "🎰" },
    Console { name: "Neo Geo Pocket", folder: "NGP", icon: "🎮" },
    Console { name: "TurboGrafx-16", folder: "PCE", icon: "🎮" },
    Console { name: "TurboGrafx CD", folder: "PCECD", icon: "💿" },
    Console { name: "Nintendo DS", folder: "NDS", icon: "📱" },
    Console { name: "Game Gear", folder: "GG", icon: "🎮" },
    Console { name: "WonderSwan", folder: "WS", icon: "🎮" },
    Console { name: "Virtual Boy", folder: "VB", icon: "🥽" },
    Console { name: "MSX", folder: "MSX", icon: "💻" },
    Console { name: "Commodore 64", folder: "COMMODORE", icon: "💻" },
    Console { name: "Amiga", folder: "AMIGA", icon: "💻" },
    Console { name: "DOS", folder: "DOS", icon: "💻" },
    Console { name: "ScummVM", folder: "SCUMMVM", icon: "🎭" },
    Console { name: "PICO-8", folder: "PICO", icon: "🎨" },
];

/// ROM extensions accepted by the library scanner, lowercase, no leading dot.
pub const ROM_EXTENSIONS: &[&str] = &[
    "gb", "gbc", "gba", "nes", "snes", "sfc", "md", "gen", "sms", "gg", "nds", "psx", "bin", "cue", "iso",
    "zip", "7z", "chd",
];

/// File extensions to exclude when scanning a states/saves directory —
/// thumbnails and RetroArch config litter, not save data.
pub const EXCLUDED_SAVE_EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "bmp", "bak", "cfg", "opt", "log", "txt", "xml", "json"];

/// Console folder → candidate RetroArch core directory names under
/// `Saves/CurrentProfile/{states,saves}/`. Checked in order after the
/// console-folder-named directory itself; if none match, the caller should
/// fall back to scanning every subdirectory.
pub const CONSOLE_CORE_NAMES: &[(&str, &[&str])] = &[
    ("GB", &["gambatte", "gearboy", "tgbdual"]),
    ("GBC", &["gambatte", "gearboy", "tgbdual"]),
    ("GBA", &["gpsp", "mgba", "vba_next"]),
    ("FC", &["fceumm", "nestopia"]),
    ("SFC", &["snes9x2005_plus", "snes9x2005", "snes9x2010", "mednafen_supafaust"]),
    ("MD", &["picodrive", "genesis_plus_gx"]),
    ("MS", &["picodrive", "smsplus", "genesis_plus_gx"]),
    ("PS", &["pcsx_rearmed"]),
    ("NDS", &["drastic"]),
    ("PCE", &["mednafen_pce_fast"]),
    ("PCECD", &["mednafen_pce_fast"]),
    ("GG", &["genesis_plus_gx", "smsplus"]),
    ("NEOGEO", &["fbalpha2012_neogeo", "fbneo"]),
    ("NGP", &["mednafen_ngp", "race"]),
    ("ARCADE", &["mame2003_plus", "fbalpha2012"]),
    ("ATARI", &["stella2014"]),
    ("LYNX", &["handy"]),
    ("WS", &["mednafen_wswan"]),
    ("VB", &["mednafen_vb"]),
    ("MSX", &["bluemsx", "fmsx"]),
    ("COMMODORE", &["vice_x64"]),
    ("AMIGA", &["puae", "uae4arm"]),
    ("DOS", &["dosbox_pure"]),
    ("PICO", &["fake08", "retro8"]),
];

/// Looks up a console by folder name (e.g. `"GBA"`).
pub fn by_folder(folder: &str) -> Option<&'static Console> {
    ALL_SYSTEMS.iter().find(|c| c.folder == folder)
}

/// Candidate RetroArch core names for a console folder, in priority order.
pub fn core_names_for(folder: &str) -> &'static [&'static str] {
    CONSOLE_CORE_NAMES.iter().find(|(f, _)| *f == folder).map(|(_, cores)| *cores).unwrap_or(&[])
}

/// True if `extension` (no leading dot, any case) is a recognized ROM type.
pub fn is_rom_extension(extension: &str) -> bool {
    let lower = extension.to_ascii_lowercase();
    ROM_EXTENSIONS.contains(&lower.as_str())
}

/// Best-guess console for a ROM extension, for the import feature's target
/// suggestion. Deliberately omits extensions that legitimately belong to more
/// than one system on this device (`bin`, `cue`, `iso`, `chd`, `zip`, `7z`) —
/// those need an explicit choice rather than a guess that's wrong half the
/// time.
pub fn console_for_extension(extension: &str) -> Option<&'static Console> {
    let folder = match extension.to_ascii_lowercase().as_str() {
        "gb" => "GB",
        "gbc" => "GBC",
        "gba" => "GBA",
        "nes" => "FC",
        "snes" | "sfc" => "SFC",
        "md" | "gen" => "MD",
        "sms" => "MS",
        "gg" => "GG",
        "nds" => "NDS",
        "psx" => "PS",
        _ => return None,
    };
    by_folder(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_console_has_a_unique_folder() {
        let mut folders: Vec<&str> = ALL_SYSTEMS.iter().map(|c| c.folder).collect();
        let before = folders.len();
        folders.sort();
        folders.dedup();
        assert_eq!(before, folders.len(), "duplicate console folder name");
    }

    #[test]
    fn gba_core_names_match_documented_table() {
        assert_eq!(core_names_for("GBA"), &["gpsp", "mgba", "vba_next"]);
    }

    #[test]
    fn unknown_console_has_no_cores() {
        assert!(core_names_for("NOPE").is_empty());
    }

    #[test]
    fn rom_extension_check_is_case_insensitive() {
        assert!(is_rom_extension("GBA"));
        assert!(is_rom_extension("gba"));
        assert!(!is_rom_extension("png"));
    }

    #[test]
    fn every_console_with_cores_is_a_known_console() {
        for (folder, _) in CONSOLE_CORE_NAMES {
            assert!(by_folder(folder).is_some(), "core table references unknown console {folder}");
        }
    }

    #[test]
    fn console_for_extension_maps_unambiguous_extensions() {
        assert_eq!(console_for_extension("gba").unwrap().folder, "GBA");
        assert_eq!(console_for_extension("GBA").unwrap().folder, "GBA");
        assert_eq!(console_for_extension("sfc").unwrap().folder, "SFC");
        assert_eq!(console_for_extension("snes").unwrap().folder, "SFC");
        assert_eq!(console_for_extension("gen").unwrap().folder, "MD");
        assert_eq!(console_for_extension("psx").unwrap().folder, "PS");
    }

    #[test]
    fn console_for_extension_refuses_to_guess_ambiguous_ones() {
        for ext in ["bin", "cue", "iso", "chd", "zip", "7z"] {
            assert!(console_for_extension(ext).is_none(), "{ext} should not resolve to a single console");
        }
    }

    #[test]
    fn every_extension_returned_by_console_for_extension_is_a_rom_extension() {
        for ext in ["gb", "gbc", "gba", "nes", "snes", "sfc", "md", "gen", "sms", "gg", "nds", "psx"] {
            assert!(console_for_extension(ext).is_some());
            assert!(is_rom_extension(ext));
        }
    }
}
