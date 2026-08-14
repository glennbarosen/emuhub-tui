//! Quirk 2 — the Miyoo writes relative `../../` segments into `rompath`
//! entries in `favourite.json` / `recentlist.json`. They must be resolved
//! before comparing against an absolute path from a directory listing, or
//! favourites will never match games.
//!
//! Ported from `FavoriteGame.normalizePath` in the Swift original:
//! split on "/", drop empties; ".." pops the last resolved component, "."
//! is skipped, anything else is pushed; rejoin with a leading "/".

/// Resolves `..`/`.` segments in a device-style path.
///
/// Unlike the Swift original (which force-pops and would crash on an
/// unbalanced `..`), a `..` with nothing to pop is silently ignored — the
/// device is not expected to produce such paths, but a TUI shouldn't panic
/// if it ever does.
pub fn normalize_path(path: &str) -> String {
    let mut resolved: Vec<&str> = Vec::new();
    for component in path.split('/').filter(|c| !c.is_empty()) {
        match component {
            ".." => {
                resolved.pop();
            }
            "." => {}
            other => resolved.push(other),
        }
    }
    format!("/{}", resolved.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_parent_segments() {
        assert_eq!(
            normalize_path("/mnt/SDCARD/Roms/GBA/../../Roms/GBA/Pokemon.gba"),
            "/mnt/SDCARD/Roms/GBA/Pokemon.gba"
        );
    }

    #[test]
    fn passes_through_already_absolute_paths() {
        assert_eq!(normalize_path("/mnt/SDCARD/Roms/GBA/Pokemon.gba"), "/mnt/SDCARD/Roms/GBA/Pokemon.gba");
    }

    #[test]
    fn drops_current_dir_segments() {
        assert_eq!(normalize_path("/mnt/./SDCARD/Roms/./GBA/g.gba"), "/mnt/SDCARD/Roms/GBA/g.gba");
    }

    #[test]
    fn collapses_double_slashes() {
        assert_eq!(normalize_path("//mnt//SDCARD//Roms"), "/mnt/SDCARD/Roms");
    }

    #[test]
    fn unbalanced_parent_does_not_panic() {
        assert_eq!(normalize_path("../../a"), "/a");
    }

    #[test]
    fn matches_captured_device_fixture() {
        // From the Swift original's doc comment example.
        assert_eq!(
            normalize_path("/mnt/SDCARD/Roms/GBA/../../Emu/GBA/../../Roms/GBA/Wario Land 4.gba"),
            "/mnt/SDCARD/Roms/GBA/Wario Land 4.gba"
        );
    }
}
