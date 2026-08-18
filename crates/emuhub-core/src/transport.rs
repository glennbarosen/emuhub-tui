//! SSH/SFTP transport to the Miyoo Mini+, in-process via `russh` +
//! `russh-sftp` rather than shelling out to `ssh`/`sftp`: a single static
//! binary, one persistent session, and real byte-level progress.
//!
//! The device runs an old dropbear. The Swift original has a commit
//! (`7d177c8 Restore SSH algorithm configuration for Miyoo compatibility`)
//! forcing cipher `aes128-ctr` and kex `diffie-hellman-group14-sha1`/`-sha256`,
//! plus accept-any host key validation (this is a personal LAN handheld, not
//! a target worth host-key pinning). Auth is `root` with an empty password;
//! some dropbear builds want the `none` method instead, so we try both.

use std::borrow::Cow;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, Handle};
use russh::keys::PublicKey;
use russh::{cipher, kex, mac, ChannelMsg, Preferred};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::favorites;
use crate::import::ImportPlan;
use crate::models::{FavoriteGame, GameFile, PlayHistoryEntry, SaveState};
use crate::{cascade, saves, scan};

/// Chunk size for streaming an upload to the device. Kept well under the
/// server's negotiated `max_write_len` (`File`'s `AsyncWrite` impl already
/// clamps to that per-call, so this is about giving the progress callback
/// frequent-enough updates, not about protocol limits.
const UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

/// How long any single SFTP round trip during an import — an existence
/// check, a `mkdir`, opening the remote file, a chunk write, or the final
/// close — may go unanswered before that file is given up on. The import
/// overlay deliberately makes `esc`/`enter` inert while a transfer is in
/// flight (see `App::confirm_import`) so a stray keypress can't abandon it
/// mid-file — which means *any* unbounded round trip in this path has no way
/// out short of killing the app, not just the byte-copy loop. Every SFTP call
/// `apply_import`/`upload_file` make is wrapped in `timeout()` for exactly
/// this reason. Same "LAN device, fail fast" reasoning as `Device::connect`'s
/// 10s timeout, just longer, since a chunk write legitimately takes real time
/// on a handheld's wifi.
const UPLOAD_STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// Result of running an `ImportPlan`: a single bad file must not abort the
/// rest of the batch (a stale/oversized ROM three files in shouldn't lose the
/// four good ones after it), so success/skip/failure are collected instead of
/// the first error short-circuiting everything.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ImportOutcome {
    pub uploaded: Vec<String>,
    /// Filenames whose destination already existed — reported rather than
    /// silently clobbered, mirroring the read-modify-write caution elsewhere
    /// in this module.
    pub skipped: Vec<String>,
    pub failed: Vec<(String, String)>,
}

const FAVORITES_PATH: &str = "/mnt/SDCARD/Roms/favourite.json";
const RECENTLIST_PATHS: &[&str] =
    &["/mnt/SDCARD/Roms/recentlist.json", "/mnt/SDCARD/Roms/recentlist-hidden.json"];

/// Bounds one SFTP round trip to `UPLOAD_STEP_TIMEOUT`, turning a device
/// that's stopped responding into a clear error instead of a wait with no
/// end. Generic over the future's error type so it wraps both the
/// `russh_sftp` calls (`try_exists`, `create_dir`, `create`, `rename` — all
/// `SftpResult<T>`) and the `tokio::io` calls on the open file
/// (`write_all`/`flush`/`shutdown` — `io::Result<()>`); `Error` already
/// converts from both.
async fn timeout<T, E>(fut: impl std::future::Future<Output = std::result::Result<T, E>>) -> Result<T>
where
    Error: From<E>,
{
    match tokio::time::timeout(UPLOAD_STEP_TIMEOUT, fut).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            Err(Error::Io(io::Error::new(io::ErrorKind::TimedOut, "device stopped responding during import")))
        }
    }
}

struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        Ok(true)
    }
}

/// A live connection to the device: one SSH session plus one SFTP subsystem
/// channel opened on top of it.
pub struct Device {
    handle: Handle<ClientHandler>,
    sftp: SftpSession,
}

impl Device {
    /// Connects, authenticates, and opens the SFTP subsystem. Fails fast
    /// (10s connect timeout) — this is a LAN device, not a WAN host.
    pub async fn connect(host: &str, port: u16, username: &str) -> Result<Self> {
        let config = Arc::new(client::Config {
            preferred: Preferred {
                kex: Cow::Borrowed(&[kex::DH_G14_SHA1, kex::DH_G14_SHA256]),
                cipher: Cow::Borrowed(&[cipher::AES_128_CTR]),
                // The device's dropbear only speaks the old hmac-sha1 MAC —
                // not in russh's modern-only default list.
                mac: Cow::Borrowed(&[mac::HMAC_SHA1, mac::HMAC_SHA256, mac::HMAC_SHA512]),
                ..Preferred::DEFAULT
            },
            ..Default::default()
        });

        let addr = format!("{host}:{port}");
        let mut handle =
            tokio::time::timeout(Duration::from_secs(10), client::connect(config, addr, ClientHandler))
                .await
                .map_err(|_| Error::ConnectTimeout)??;

        let authenticated = handle.authenticate_password(username, "").await?;
        if !authenticated.success() {
            let authenticated = handle.authenticate_none(username).await?;
            if !authenticated.success() {
                return Err(Error::AuthFailed);
            }
        }

        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;

        Ok(Self { handle, sftp })
    }

    /// Runs an arbitrary command on an exec channel and returns its stdout.
    /// Exposed (not just used internally) so a future headless CLI mode
    /// (`emuhub ls GBA`) and ad-hoc device debugging don't need a new
    /// transport method for every one-off query.
    pub async fn exec(&self, command: &str) -> Result<String> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut output = Vec::new();
        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => output.extend_from_slice(&data),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            }
        }
        Ok(String::from_utf8_lossy(&output).into_owned())
    }

    /// One round trip for the whole library — see module docs and
    /// `scan::parse_find_output`.
    ///
    /// Asks `stat` for each file's byte size in the same pass, since a bare
    /// `find` doesn't give it and per-file `stat`s would undo the whole
    /// one-round-trip design. Onion's busybox is old enough that neither
    /// `stat -c` nor `-exec … +` is guaranteed, so an empty result falls back
    /// to the plain listing: sizes are a nice-to-have in the detail pane, the
    /// library is not. `parse_find_output` accepts both shapes, so the
    /// fallback is just a different command, not a second parser.
    pub async fn list_all_roms(&self) -> Result<Vec<GameFile>> {
        let with_sizes = self
            .exec("find /mnt/SDCARD/Roms -maxdepth 2 -type f -exec stat -c '%s %n' {} + 2>/dev/null")
            .await?;
        let games = scan::parse_find_output(&with_sizes);
        if !games.is_empty() {
            return Ok(games);
        }

        tracing::debug!("stat-augmented listing came back empty, falling back to plain find");
        let output = self.exec("find /mnt/SDCARD/Roms -maxdepth 2 -type f").await?;
        Ok(scan::parse_find_output(&output))
    }

    /// The whole save tree in one round trip, same trick as `list_all_roms`:
    /// parsing locally is far cheaper than probing each console's candidate
    /// core directories over SFTP, and it means a per-game lookup afterwards
    /// costs nothing at all.
    ///
    /// `2>/dev/null` because a card that has never run a given emulator simply
    /// won't have one of these directories, and `find` would otherwise put a
    /// diagnostic on stderr and exit non-zero.
    pub async fn list_save_states(&self) -> Result<Vec<SaveState>> {
        let command = format!("find {} {} -type f 2>/dev/null", saves::STATES_ROOT, saves::SAVES_ROOT);
        let output = self.exec(&command).await?;
        Ok(saves::parse_save_listing(&output))
    }

    async fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        Ok(self.sftp.read(path).await?)
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<()> {
        self.sftp.write(path, data).await?;
        Ok(())
    }

    pub async fn fetch_image(&self, remote_path: &str) -> Option<Vec<u8>> {
        self.read_file(remote_path).await.ok()
    }

    pub async fn read_favorites(&self) -> Result<Vec<FavoriteGame>> {
        match self.read_file(FAVORITES_PATH).await {
            Ok(bytes) => Ok(favorites::parse_favorites(&String::from_utf8_lossy(&bytes))),
            Err(_) => Ok(Vec::new()), // absent file == no favourites yet, not an error
        }
    }

    /// Read-modify-write with a `.bak` copy taken first — every write here
    /// lands on a running handheld's live SD card, so it has to be reversible.
    pub async fn write_favorites(&self, favorites: &[FavoriteGame]) -> Result<()> {
        if let Ok(existing) = self.read_file(FAVORITES_PATH).await {
            let _ = self.write_file(&format!("{FAVORITES_PATH}.bak"), &existing).await;
        }
        let text = crate::favorites::write_favorites(favorites)?;
        self.write_file(FAVORITES_PATH, text.as_bytes()).await
    }

    /// Writes the recent list back, `.bak` first — same read-modify-write
    /// contract as `write_favorites`, and the same NDJSON format (Quirk 1).
    ///
    /// Writes to whichever of the two recent-list filenames the device
    /// actually has, since Onion uses `recentlist-hidden.json` when the recent
    /// list is hidden in its settings and writing the other one would have no
    /// visible effect.
    pub async fn write_recents(&self, recents: &[PlayHistoryEntry]) -> Result<()> {
        let path = self.recentlist_path().await;
        if let Ok(existing) = self.read_file(&path).await {
            let _ = self.write_file(&format!("{path}.bak"), &existing).await;
        }
        let text = favorites::write_ndjson(recents)?;
        self.write_file(&path, text.as_bytes()).await
    }

    /// The recent-list file this device is actually using, falling back to the
    /// standard name when neither exists yet.
    async fn recentlist_path(&self) -> String {
        for path in RECENTLIST_PATHS {
            if self.sftp.try_exists(*path).await.unwrap_or(false) {
                return (*path).to_string();
            }
        }
        RECENTLIST_PATHS[0].to_string()
    }

    /// Removes a file, treating "it wasn't there" as success.
    ///
    /// A cascade plans for box art and saves that may legitimately not exist
    /// — most ROMs have no box art — so a missing file is the expected case,
    /// not a failure that should abort the rest of the delete.
    pub async fn remove_file(&self, path: &str) -> Result<()> {
        match self.sftp.remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.sftp.try_exists(path).await.unwrap_or(false) {
                    Err(err.into())
                } else {
                    tracing::debug!(%path, "nothing to remove");
                    Ok(())
                }
            }
        }
    }

    /// Renames a file, treating a missing source as success for the same
    /// reason `remove_file` does.
    pub async fn rename_file(&self, from: &str, to: &str) -> Result<()> {
        match self.sftp.rename(from, to).await {
            Ok(()) => Ok(()),
            Err(err) => {
                if self.sftp.try_exists(from).await.unwrap_or(false) {
                    Err(err.into())
                } else {
                    tracing::debug!(%from, "nothing to rename");
                    Ok(())
                }
            }
        }
    }

    pub async fn read_recents(&self) -> Result<Vec<PlayHistoryEntry>> {
        for path in RECENTLIST_PATHS {
            if let Ok(bytes) = self.read_file(path).await {
                let entries = favorites::parse_recents(&String::from_utf8_lossy(&bytes));
                if !entries.is_empty() {
                    return Ok(entries);
                }
            }
        }
        Ok(Vec::new())
    }

    /// Executes a `DeletePlan`: removes every planned file, then rewrites the
    /// lists that referenced the game.
    ///
    /// Order matters. The ROM goes first so that a failure part-way through
    /// can't leave the game playable but stripped of its saves, and the list
    /// rewrites go last so `favourite.json` is only edited once the files it
    /// pointed at are actually gone.
    pub async fn apply_delete(&self, plan: &cascade::DeletePlan) -> Result<()> {
        for path in &plan.removals {
            self.remove_file(path).await?;
        }
        if let Some(favorites) = &plan.favorites {
            self.write_favorites(favorites).await?;
        }
        if let Some(recents) = &plan.recents {
            self.write_recents(recents).await?;
        }
        self.reset_console_cache(&plan.console_cache_db).await;
        Ok(())
    }

    /// Executes a `RenamePlan`, same ordering rationale as `apply_delete`.
    pub async fn apply_rename(&self, plan: &cascade::RenamePlan) -> Result<()> {
        for (from, to) in &plan.renames {
            self.rename_file(from, to).await?;
        }
        if let Some(favorites) = &plan.favorites {
            self.write_favorites(favorites).await?;
        }
        if let Some(recents) = &plan.recents {
            self.write_recents(recents).await?;
        }
        self.reset_console_cache(&plan.console_cache_db).await;
        Ok(())
    }

    /// Uploads a local file to the device, streaming it in fixed-size chunks
    /// rather than buffering the whole thing like `write_file` does — a
    /// PS1/N64 image can run several hundred MB, well past what belongs in
    /// one in-memory `Vec`.
    ///
    /// Writes to `{remote}.part` and only renames into place once every byte
    /// has landed, so an interrupted transfer (dropped wifi, closed laptop
    /// lid) can never leave a truncated file sitting where the device's own
    /// scanner would find it and list it as a playable ROM.
    async fn upload_file(&self, local: &Path, remote: &str, mut on_progress: impl FnMut(u64)) -> Result<()> {
        let partial = format!("{remote}.part");
        let mut source = tokio::fs::File::open(local).await?;
        let mut dest = timeout(self.sftp.create(&partial)).await?;

        let mut buf = vec![0u8; UPLOAD_CHUNK_BYTES];
        let mut sent = 0u64;
        loop {
            let n = source.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            timeout(dest.write_all(&buf[..n])).await?;
            // Flushing per chunk is what makes `on_progress` mean "sent and
            // acknowledged by the device", not just "handed to the local
            // write queue". `File::poll_write` queues a write and only waits
            // for its SFTP status reply here (or on shutdown) — without this,
            // reading a ROM off local disk is fast enough that the gauge
            // jumps to 100% almost immediately and then appears to freeze
            // while `shutdown()` drains every unacknowledged write over the
            // wifi link at the very end.
            timeout(dest.flush()).await?;
            sent += n as u64;
            on_progress(sent);
        }
        timeout(dest.shutdown()).await?;
        drop(dest);

        timeout(self.sftp.rename(&partial, remote)).await?;
        Ok(())
    }

    /// Runs an `ImportPlan`: uploads every job, skipping (not clobbering) a
    /// destination that already exists, then resets the Onion ROM cache for
    /// every console that received a new file.
    ///
    /// That reset is the same `console_cache_db`/`reset_console_cache`
    /// mechanism `apply_delete`/`apply_rename` already use — the cache is a
    /// snapshot of a console's ROM listing, so it goes stale on an addition
    /// exactly as it does on a deletion or rename; nothing here is special to
    /// imports.
    ///
    /// `on_progress(job_index, total_jobs, filename, bytes_sent, file_size)`
    /// is called as each file streams, so the UI can drive a gauge without
    /// waiting for the whole batch.
    pub async fn apply_import(
        &self,
        plan: &ImportPlan,
        mut on_progress: impl FnMut(usize, usize, &str, u64, u64),
    ) -> ImportOutcome {
        let mut outcome = ImportOutcome::default();
        let mut touched_consoles: Vec<&str> = Vec::new();
        let total = plan.jobs.len();

        for (index, job) in plan.jobs.iter().enumerate() {
            match timeout(self.sftp.try_exists(job.remote_path.as_str())).await {
                Ok(true) => {
                    outcome.skipped.push(job.filename.clone());
                    continue;
                }
                Ok(false) => {}
                Err(err) => {
                    outcome.failed.push((job.filename.clone(), err.to_string()));
                    continue;
                }
            }

            let console_dir = format!("/mnt/SDCARD/Roms/{}", job.console_folder);
            let dir_exists = timeout(self.sftp.try_exists(console_dir.as_str())).await.unwrap_or(true);
            if !dir_exists {
                if let Err(err) = timeout(self.sftp.create_dir(console_dir.as_str())).await {
                    outcome.failed.push((job.filename.clone(), err.to_string()));
                    continue;
                }
            }

            let filename = job.filename.clone();
            let file_size = job.size;
            let upload_result = self
                .upload_file(&job.local_path, &job.remote_path, |bytes_sent| {
                    on_progress(index, total, &filename, bytes_sent, file_size);
                })
                .await;

            match upload_result {
                Ok(()) => {
                    outcome.uploaded.push(job.filename.clone());
                    if !touched_consoles.contains(&job.console_folder.as_str()) {
                        touched_consoles.push(&job.console_folder);
                    }
                }
                Err(err) => outcome.failed.push((job.filename.clone(), err.to_string())),
            }
        }

        for folder in touched_consoles {
            self.reset_console_cache(&cascade::console_cache_db(folder)).await;
        }

        outcome
    }

    /// Moves Onion's per-console ROM cache aside so it gets rebuilt on the
    /// next scan, instead of continuing to list a ROM that no longer exists
    /// under that name.
    ///
    /// Renamed rather than deleted, and failures are logged rather than
    /// propagated: this is a cache the device owns and regenerates, so a
    /// stale entry in the handheld's menu is a cosmetic problem, while
    /// failing the whole operation over it would be a real one. The `.bak`
    /// also means the original is recoverable if the rebuild misbehaves.
    async fn reset_console_cache(&self, path: &str) {
        if let Err(err) = self.rename_file(path, &format!("{path}.bak")).await {
            tracing::warn!(%err, %path, "couldn't reset Onion's ROM cache; its menu may show a stale entry until it rescans");
        }
    }
}
