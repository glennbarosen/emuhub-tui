//! XDG-path cache: config at `~/.config/emuhub/`, library/favourites cache
//! at `~/.local/state/emuhub/`, box-art/thumbnail images at
//! `~/.cache/emuhub/` — XDG paths, not the `applicationSupportDirectory` the
//! Swift original used.
//!
//! Two-tier design ported from the Swift `CacheManager`: an in-memory
//! `AppCache` the caller mutates, explicitly flushed to disk with `save()`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;

use crate::models::{AppCache, DeviceConfig};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "emuhub";

#[derive(Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Resolves XDG directories for this platform. Falls back to
    /// `state_dir = data_local_dir` on platforms without a distinct XDG
    /// state directory (not expected on Linux, but keeps this portable).
    pub fn resolve() -> anyhow::Result<Self> {
        let dirs = ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
            .ok_or_else(|| anyhow::anyhow!("could not determine home directory"))?;
        let state_dir =
            dirs.state_dir().map(Path::to_path_buf).unwrap_or_else(|| dirs.data_local_dir().to_path_buf());

        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            state_dir,
            cache_dir: dirs.cache_dir().to_path_buf(),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn library_cache_file(&self) -> PathBuf {
        self.state_dir.join("library.json")
    }

    pub fn images_dir(&self) -> PathBuf {
        self.cache_dir.join("images")
    }

    fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(&self.state_dir)?;
        std::fs::create_dir_all(self.images_dir())?;
        Ok(())
    }
}

/// Loads `config.toml`, returning `DeviceConfig::default()` (empty host) if
/// it doesn't exist yet.
pub fn load_config(paths: &Paths) -> anyhow::Result<DeviceConfig> {
    let file = paths.config_file();
    if !file.exists() {
        return Ok(DeviceConfig::default());
    }
    let text = std::fs::read_to_string(&file)?;
    Ok(toml::from_str(&text)?)
}

pub fn save_config(paths: &Paths, config: &DeviceConfig) -> anyhow::Result<()> {
    paths.ensure_dirs()?;
    let text = toml::to_string_pretty(config)?;
    std::fs::write(paths.config_file(), text)?;
    Ok(())
}

/// Loads the cached library/favourites, returning an empty `AppCache` if no
/// cache file exists yet (first run, or cache was cleared).
pub fn load_cache(paths: &Paths) -> AppCache {
    let file = paths.library_cache_file();
    let Ok(text) = std::fs::read_to_string(&file) else {
        return AppCache::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|err| {
        tracing::warn!(%err, "cache file corrupt, starting fresh");
        AppCache::default()
    })
}

pub fn save_cache(paths: &Paths, cache: &AppCache) -> anyhow::Result<()> {
    paths.ensure_dirs()?;
    let text = serde_json::to_string_pretty(cache)?;
    std::fs::write(paths.library_cache_file(), text)?;
    Ok(())
}

/// Seconds since the unix epoch, for stamping `AppCache::last_full_sync`.
/// Saturates at 0 rather than erroring — a clock set before 1970 is not worth
/// a `Result` on the path that records "the library scan finished".
pub fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Renders a cache timestamp as an age relative to `now`, for the status
/// line's `offline (cache · synced 2h ago)`.
///
/// A cache stamped in the future (clock skew, or a card moved between
/// machines) reads as "just now" rather than a negative age.
pub fn relative_age(then: u64, now: u64) -> String {
    let secs = now.saturating_sub(then);
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

/// Maps a remote path to a flat local cache filename (device paths contain
/// `/`, which can't appear in a single filename).
///
/// Literal `_` is escaped to `__` before `/` is collapsed to a single `_` —
/// without that, a remote path containing a literal underscore where another
/// path has a slash could flatten to the same filename (e.g. `"a_b"` and
/// `"a/b"` both naively becoming `"a_b"`), aliasing one game's cached image
/// under another's key.
fn cache_filename(remote_path: &str) -> String {
    remote_path.replace('_', "__").replace('/', "_")
}

pub fn cached_image_path(paths: &Paths, remote_path: &str) -> PathBuf {
    paths.images_dir().join(cache_filename(remote_path))
}

pub fn cache_image(paths: &Paths, remote_path: &str, data: &[u8]) -> anyhow::Result<()> {
    paths.ensure_dirs()?;
    std::fs::write(cached_image_path(paths, remote_path), data)?;
    Ok(())
}

pub fn get_cached_image(paths: &Paths, remote_path: &str) -> Option<Vec<u8>> {
    let path = cached_image_path(paths, remote_path);
    let data = std::fs::read(&path).ok()?;
    touch(&path);
    Some(data)
}

/// Bumps a cache file's mtime to now, so `prune_image_cache` evicts by *last
/// use* rather than by first fetch. Cached images are never rewritten — without
/// this, the box art you look at every day ages out exactly as fast as art you
/// saw once.
///
/// Best-effort on purpose: a read-only cache directory must degrade to a
/// worse eviction order, never to missing box art.
fn touch(path: &Path) {
    let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) else {
        return;
    };
    let _ = file.set_modified(SystemTime::now());
}

/// Evicts oldest-first from the image cache until it fits in `max_bytes`,
/// returning `(files removed, bytes freed)`.
///
/// "Oldest" is by mtime, which `touch` keeps as a last-used stamp. A cap of 0
/// empties the cache; a missing cache directory is a no-op, not an error.
pub fn prune_image_cache(paths: &Paths, max_bytes: u64) -> anyhow::Result<(usize, u64)> {
    let dir = paths.images_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok((0, 0));
    };

    let mut files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        total += meta.len();
        files.push((entry.path(), meta.len(), modified));
    }

    if total <= max_bytes {
        return Ok((0, 0));
    }

    files.sort_by_key(|(_, _, modified)| *modified);
    let mut removed = 0usize;
    let mut freed = 0u64;
    for (path, size, _) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_err() {
            continue;
        }
        total -= size;
        freed += size;
        removed += 1;
    }
    Ok((removed, freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{CachedConsole, GameFile};

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
        };
        (dir, paths)
    }

    #[test]
    fn missing_config_yields_default() {
        let (_dir, paths) = temp_paths();
        let cfg = load_config(&paths).unwrap();
        assert_eq!(cfg.host, "");
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.username, "root");
    }

    #[test]
    fn config_round_trips() {
        let (_dir, paths) = temp_paths();
        let cfg = DeviceConfig {
            host: "192.168.68.55".into(),
            port: 22,
            username: "root".into(),
            image_cache_max_mb: 200,
        };
        save_config(&paths, &cfg).unwrap();
        let loaded = load_config(&paths).unwrap();
        assert_eq!(loaded.host, cfg.host);
        assert_eq!(loaded.image_cache_max_mb, 200);
    }

    #[test]
    fn config_without_the_image_cache_field_still_loads() {
        // Config files written before the cap existed must keep working.
        let (_dir, paths) = temp_paths();
        paths.ensure_dirs().unwrap();
        std::fs::write(paths.config_file(), "host = \"192.168.68.55\"\n").unwrap();

        let loaded = load_config(&paths).unwrap();
        assert_eq!(loaded.host, "192.168.68.55");
        assert_eq!(loaded.image_cache_max_mb, 200);
    }

    #[test]
    fn missing_cache_yields_empty_default() {
        let (_dir, paths) = temp_paths();
        let cache = load_cache(&paths);
        assert!(cache.consoles.is_empty());
        assert!(cache.favorites.is_empty());
    }

    #[test]
    fn cache_round_trips() {
        let (_dir, paths) = temp_paths();
        let game = GameFile {
            path: "/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba".into(),
            name: "Metroid Fusion.gba".into(),
            console_folder: "GBA".into(),
            extension: "gba".into(),
            size: Some(1024),
            image_path: "/mnt/SDCARD/Roms/GBA/Imgs/Metroid Fusion.png".into(),
        };
        let mut cache = AppCache::default();
        cache.consoles.push(CachedConsole {
            folder: "GBA".into(),
            name: "Game Boy Advance".into(),
            games: vec![game],
        });
        cache.favorites.push("/mnt/SDCARD/Roms/GBA/Metroid Fusion.gba".into());

        save_cache(&paths, &cache).unwrap();
        let loaded = load_cache(&paths);
        assert_eq!(loaded.consoles.len(), 1);
        assert_eq!(loaded.consoles[0].games.len(), 1);
        assert_eq!(loaded.favorites, cache.favorites);
    }

    #[test]
    fn corrupt_cache_falls_back_to_empty() {
        let (_dir, paths) = temp_paths();
        paths.ensure_dirs().unwrap();
        std::fs::write(paths.library_cache_file(), "not json").unwrap();
        let cache = load_cache(&paths);
        assert!(cache.consoles.is_empty());
    }

    #[test]
    fn image_cache_round_trips_and_flattens_path() {
        let (_dir, paths) = temp_paths();
        let remote = "/mnt/SDCARD/Roms/GBA/Imgs/Metroid Fusion.png";
        assert!(get_cached_image(&paths, remote).is_none());

        cache_image(&paths, remote, b"PNGDATA").unwrap();
        assert_eq!(get_cached_image(&paths, remote).unwrap(), b"PNGDATA");

        let path = cached_image_path(&paths, remote);
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!filename.contains('/'), "flattened filename must not contain a path separator");
        assert!(filename.starts_with("_mnt_SDCARD"));
    }

    #[test]
    fn cache_filename_does_not_collide_on_literal_underscore_vs_slash() {
        assert_ne!(cache_filename("/mnt/SDCARD/a_b"), cache_filename("/mnt/SDCARD/a/b"));
    }

    #[test]
    fn relative_age_buckets() {
        assert_eq!(relative_age(1000, 1000), "just now");
        assert_eq!(relative_age(1000, 1059), "just now");
        assert_eq!(relative_age(0, 60), "1m ago");
        assert_eq!(relative_age(0, 3599), "59m ago");
        assert_eq!(relative_age(0, 3600), "1h ago");
        assert_eq!(relative_age(0, 86_399), "23h ago");
        assert_eq!(relative_age(0, 86_400), "1d ago");
        assert_eq!(relative_age(0, 5 * 86_400), "5d ago");
    }

    #[test]
    fn relative_age_never_goes_backwards() {
        // Cache stamped in the future (clock skew) reads as fresh, not negative.
        assert_eq!(relative_age(2000, 1000), "just now");
    }

    /// Writes an image into the cache with an explicit mtime, so eviction
    /// order is deterministic rather than dependent on filesystem timing.
    fn write_aged_image(paths: &Paths, remote: &str, data: &[u8], age_secs: u64) {
        cache_image(paths, remote, data).unwrap();
        let path = cached_image_path(paths, remote);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - std::time::Duration::from_secs(age_secs)).unwrap();
    }

    #[test]
    fn prune_under_cap_is_a_noop() {
        let (_dir, paths) = temp_paths();
        write_aged_image(&paths, "/a.png", &[0u8; 100], 10);

        let (removed, freed) = prune_image_cache(&paths, 1000).unwrap();
        assert_eq!((removed, freed), (0, 0));
        assert!(get_cached_image(&paths, "/a.png").is_some());
    }

    #[test]
    fn prune_evicts_oldest_first_until_under_cap() {
        let (_dir, paths) = temp_paths();
        write_aged_image(&paths, "/old.png", &[0u8; 100], 3600);
        write_aged_image(&paths, "/middle.png", &[0u8; 100], 60);
        write_aged_image(&paths, "/new.png", &[0u8; 100], 1);

        // Cap fits one file, so the two oldest must go.
        let (removed, freed) = prune_image_cache(&paths, 150).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(freed, 200);
        assert!(get_cached_image(&paths, "/old.png").is_none());
        assert!(get_cached_image(&paths, "/middle.png").is_none());
        assert!(get_cached_image(&paths, "/new.png").is_some());
    }

    #[test]
    fn prune_missing_cache_dir_is_not_an_error() {
        let (_dir, paths) = temp_paths();
        assert_eq!(prune_image_cache(&paths, 100).unwrap(), (0, 0));
    }

    #[test]
    fn reading_a_cached_image_refreshes_its_eviction_stamp() {
        let (_dir, paths) = temp_paths();
        write_aged_image(&paths, "/stale-but-used.png", &[0u8; 100], 3600);
        write_aged_image(&paths, "/newer.png", &[0u8; 100], 60);

        // A hit on the older file makes it the most recently used, so the
        // file that was written later is the one that gets evicted.
        assert!(get_cached_image(&paths, "/stale-but-used.png").is_some());

        prune_image_cache(&paths, 150).unwrap();
        assert!(get_cached_image(&paths, "/stale-but-used.png").is_some());
        assert!(get_cached_image(&paths, "/newer.png").is_none());
    }
}
