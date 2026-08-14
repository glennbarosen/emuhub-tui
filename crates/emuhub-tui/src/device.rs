//! The Device task: owns the `russh` session and SFTP
//! subsystem, serializes every SFTP op, and reports connection state back
//! to the UI over a channel. The UI never awaits network I/O directly.

use std::collections::VecDeque;

use emuhub_core::cache::{self, Paths};
use emuhub_core::cascade::{DeletePlan, RenamePlan};
use emuhub_core::discover;
use emuhub_core::models::{DeviceConfig, FavoriteGame, GameFile, PlayHistoryEntry, SaveState};
use emuhub_core::transport::Device;
use tokio::sync::mpsc;

pub enum DeviceRequest {
    Connect,
    /// Updates the device task's configured host and (re)connects to it,
    /// dropping any existing session — used when the user changes the IP
    /// from the in-app prompt rather than a CLI arg at startup.
    SetHost(String),
    /// Full favourites list to write back (UI has already applied the
    /// toggle locally and rebuilt this list — see `App::toggle_favorite`).
    SyncFavorites(Vec<FavoriteGame>),
    /// `key` is what the UI files the decoded image under — a ROM path for
    /// box art, a save-state thumbnail path for the saves browser;
    /// `image_path` is where the bytes actually live on device/in the disk
    /// cache. Kept distinct — conflating them was the box-art-never-shows
    /// bug: the UI compared the fetched image's own path against the ROM's
    /// path and the two can never be equal.
    FetchImage {
        key: String,
        image_path: String,
    },
    /// Re-scan everything over the *existing* session: library, favourites,
    /// recents, save states. Distinct from `Connect`, which throws the session
    /// away and redials — a card whose contents changed on the handheld
    /// doesn't need a new SSH handshake, and reconnecting to pick up one new
    /// ROM is both slower and racier.
    RefreshLibrary,
    /// Re-run the save-tree listing. The connect path already does this once,
    /// but a session that started with the device asleep would otherwise have
    /// no way back — every game reports "no save states" for the rest of the
    /// session with nothing to retry.
    LoadSaveStates,
    /// Sweep the local /24 for the handheld. Runs off the request loop (see
    /// `run`) so it can't stall image fetches for the duration.
    Discover,
    /// Execute a cascade the user has already seen and confirmed. The plan is
    /// computed UI-side so the confirmation dialog and the device act on the
    /// exact same set of paths — there is no second, divergent derivation.
    DeleteGame {
        game: GameFile,
        plan: Box<DeletePlan>,
    },
    RenameGame {
        game: GameFile,
        plan: Box<RenamePlan>,
    },
    Shutdown,
}

pub enum DeviceEvent {
    Connecting,
    Connected,
    ConnectFailed(String),
    LibraryLoaded(Vec<(&'static str, Vec<GameFile>)>),
    FavoritesLoaded(Vec<FavoriteGame>),
    RecentsLoaded(Vec<PlayHistoryEntry>),
    /// Every save state on the card, flat. The UI indexes it per game via
    /// `saves::states_for_game` — one listing serves the whole session.
    SaveStatesLoaded(Vec<SaveState>),
    FavoritesSynced,
    ImageBytes {
        key: String,
        data: Vec<u8>,
    },
    /// The range about to be swept, e.g. `192.168.68.0/22`.
    DiscoveryStarted {
        network: String,
    },
    DiscoveryProgress {
        done: usize,
        total: usize,
    },
    DiscoveryFound(String),
    /// A host with SSH open that failed the `/mnt/SDCARD` identity check.
    DiscoveryRejected {
        host: String,
        reason: String,
    },
    DiscoveryDone,
    /// The cascade landed on the device; the UI can now update its indices.
    GameDeleted {
        game: GameFile,
        plan: Box<DeletePlan>,
    },
    GameRenamed {
        game: GameFile,
        plan: Box<RenamePlan>,
    },
    Error(String),
}

pub async fn run(
    config: DeviceConfig,
    paths: Paths,
    mut requests: mpsc::UnboundedReceiver<DeviceRequest>,
    events: mpsc::UnboundedSender<DeviceEvent>,
) {
    let mut config = config;
    let mut device: Option<Device> = None;
    let mut pending: VecDeque<DeviceRequest> = VecDeque::new();

    loop {
        // Block until there's something to do, then take everything else
        // that's already queued behind it. Draining first is what makes
        // `coalesce_image_fetches` possible: the backlog has to be visible
        // all at once before the stale entries in it can be dropped.
        if pending.is_empty() {
            match requests.recv().await {
                Some(request) => pending.push_back(request),
                None => break,
            }
        }
        while let Ok(request) = requests.try_recv() {
            pending.push_back(request);
        }
        coalesce_image_fetches(&mut pending);

        let Some(request) = pending.pop_front() else {
            continue;
        };
        match request {
            DeviceRequest::Connect => {
                device = do_connect(&config, &events).await;
            }
            DeviceRequest::SetHost(host) => {
                config.host = host;
                // Reassigning drops any existing session before dialing the
                // new host — requests are processed strictly sequentially
                // here, so there's no in-flight `Connect` this could race.
                device = do_connect(&config, &events).await;
            }
            DeviceRequest::SyncFavorites(favorites) => {
                let Some(d) = &device else {
                    let _ = events
                        .send(DeviceEvent::Error("not connected — favourite queued for next sync".into()));
                    continue;
                };
                match d.write_favorites(&favorites).await {
                    Ok(()) => {
                        let _ = events.send(DeviceEvent::FavoritesSynced);
                    }
                    Err(err) => {
                        let _ = events.send(DeviceEvent::Error(format!("failed to sync favourites: {err}")));
                    }
                }
            }
            DeviceRequest::FetchImage { key, image_path } => {
                // Disk cache first — box art rarely changes once downloaded.
                if let Some(data) = cache::get_cached_image(&paths, &image_path) {
                    let _ = events.send(DeviceEvent::ImageBytes { key, data });
                    continue;
                }
                let Some(d) = &device else { continue };
                if let Some(data) = d.fetch_image(&image_path).await {
                    if let Err(err) = cache::cache_image(&paths, &image_path, &data) {
                        tracing::warn!(%err, %image_path, "failed to cache box art to disk");
                    }
                    let _ = events.send(DeviceEvent::ImageBytes { key, data });
                } else {
                    tracing::debug!(%image_path, "box art not found on device");
                }
            }
            DeviceRequest::DeleteGame { game, plan } => {
                // Unlike a favourite toggle, this cannot be queued for the
                // next sync: the UI must not drop the game from the browser
                // while it still exists on the card.
                let Some(d) = &device else {
                    let _ = events.send(DeviceEvent::Error("not connected — can't delete".into()));
                    continue;
                };
                match d.apply_delete(&plan).await {
                    Ok(()) => {
                        let _ = events.send(DeviceEvent::GameDeleted { game, plan });
                    }
                    Err(err) => {
                        let _ = events.send(DeviceEvent::Error(format!("delete failed: {err}")));
                    }
                }
            }
            DeviceRequest::RenameGame { game, plan } => {
                let Some(d) = &device else {
                    let _ = events.send(DeviceEvent::Error("not connected — can't rename".into()));
                    continue;
                };
                match d.apply_rename(&plan).await {
                    Ok(()) => {
                        let _ = events.send(DeviceEvent::GameRenamed { game, plan });
                    }
                    Err(err) => {
                        let _ = events.send(DeviceEvent::Error(format!("rename failed: {err}")));
                    }
                }
            }
            DeviceRequest::RefreshLibrary => {
                let Some(d) = &device else {
                    let _ = events
                        .send(DeviceEvent::Error("not connected — can't refresh (s → Reconnect)".into()));
                    continue;
                };
                // Ends with the save-tree listing itself, so this is the full
                // connect-time reload minus the handshake.
                load_library_and_favorites(d, &events).await;
            }
            DeviceRequest::LoadSaveStates => {
                let Some(d) = &device else {
                    let _ = events.send(DeviceEvent::Error("not connected — can't load save states".into()));
                    continue;
                };
                load_save_states(d, &events).await;
            }
            DeviceRequest::Discover => {
                // Deliberately spawned rather than awaited here: this loop
                // handles requests strictly one at a time, and a 254-host
                // sweep takes seconds. Blocking it would freeze box-art
                // fetches and favourite syncs for the whole scan.
                let events = events.clone();
                let username = config.username.clone();
                let port = config.port;
                tokio::spawn(async move { run_discovery(port, &username, events).await });
            }
            DeviceRequest::Shutdown => break,
        }
    }
}

fn is_image_fetch(request: &DeviceRequest) -> bool {
    matches!(request, DeviceRequest::FetchImage { .. })
}

/// Drops every queued image fetch but the newest.
///
/// Each keypress in a list fires a fetch for the newly selected row, and each
/// fetch is a full SFTP round trip to a handheld over wifi. Held in a strict
/// FIFO, scrolling eight rows down meant the image you were actually looking
/// at was fetched *last*, behind seven images for rows you'd already left —
/// so the preview took seconds to appear even though any single fetch is
/// fast. Only the newest can ever be displayed (`App::image_state` is a
/// single slot), so the rest are pure latency.
///
/// Dropping them is safe precisely because there is no "already requested"
/// gate: navigating back re-requests, and the disk cache serves it instantly.
fn coalesce_image_fetches(pending: &mut VecDeque<DeviceRequest>) {
    let Some(newest) = pending.iter().rposition(is_image_fetch) else {
        return;
    };

    let mut idx = 0;
    pending.retain(|request| {
        let keep = idx == newest || !is_image_fetch(request);
        idx += 1;
        keep
    });
}

async fn run_discovery(port: u16, username: &str, events: mpsc::UnboundedSender<DeviceEvent>) {
    let Some(network) = discover::local_network() else {
        let _ = events.send(DeviceEvent::Error("couldn't determine the local subnet".into()));
        let _ = events.send(DeviceEvent::DiscoveryDone);
        return;
    };

    // The swept range goes to the UI before the first probe: when a device
    // that is plainly switched on isn't found, "we searched 192.168.68.0/22"
    // is the one fact that tells the user whether the scan was even looking
    // in the right place.
    let _ = events.send(DeviceEvent::DiscoveryStarted { network: network.label() });

    discover::scan(network, port, username, |update| {
        let event = match update {
            discover::DiscoveryUpdate::Progress { done, total } => {
                DeviceEvent::DiscoveryProgress { done, total }
            }
            discover::DiscoveryUpdate::Found { host } => DeviceEvent::DiscoveryFound(host),
            discover::DiscoveryUpdate::Rejected { host, reason } => {
                DeviceEvent::DiscoveryRejected { host, reason }
            }
            discover::DiscoveryUpdate::Done => DeviceEvent::DiscoveryDone,
        };
        let _ = events.send(event);
    })
    .await;
}

/// Shared by `Connect` and `SetHost`: reports `Connecting`, dials the
/// device, and on success loads the library/favourites before handing back
/// the live session for the caller to store.
async fn do_connect(config: &DeviceConfig, events: &mpsc::UnboundedSender<DeviceEvent>) -> Option<Device> {
    if config.host.is_empty() {
        let _ = events.send(DeviceEvent::ConnectFailed("no host configured".into()));
        return None;
    }
    let _ = events.send(DeviceEvent::Connecting);
    match Device::connect(&config.host, config.port, &config.username).await {
        Ok(d) => {
            let _ = events.send(DeviceEvent::Connected);
            load_library_and_favorites(&d, events).await;
            Some(d)
        }
        Err(err) => {
            let _ = events.send(DeviceEvent::ConnectFailed(err.to_string()));
            None
        }
    }
}

async fn load_library_and_favorites(device: &Device, events: &mpsc::UnboundedSender<DeviceEvent>) {
    match device.list_all_roms().await {
        Ok(games) => {
            let grouped = emuhub_core::scan::group_by_console(games);
            let _ = events.send(DeviceEvent::LibraryLoaded(grouped));
        }
        Err(err) => {
            let _ = events.send(DeviceEvent::Error(format!("failed to list ROMs: {err}")));
        }
    }

    match device.read_favorites().await {
        Ok(favorites) => {
            let _ = events.send(DeviceEvent::FavoritesLoaded(favorites));
        }
        Err(err) => {
            let _ = events.send(DeviceEvent::Error(format!("failed to read favourites: {err}")));
        }
    }

    // A missing/unreadable recent list is not worth an error banner — the
    // device simply may not have one yet. `read_recents` already treats an
    // absent file as empty, so anything reaching here is a real transport
    // failure and belongs in the log, not over the user's status line.
    match device.read_recents().await {
        Ok(recents) => {
            let _ = events.send(DeviceEvent::RecentsLoaded(recents));
        }
        Err(err) => {
            tracing::warn!(%err, "failed to read play history");
        }
    }

    load_save_states(device, events).await;
}

/// The save-tree listing, shared by the connect path and the on-demand
/// refresh.
///
/// A failure here used to go to `tracing::warn!`, which writes to stderr
/// underneath the alternate screen with the env filter off by default — so a
/// listing that never arrived was indistinguishable from a card with no saves,
/// and every game reported "no save states" with no way to tell why. It gets a
/// real error event now.
async fn load_save_states(device: &Device, events: &mpsc::UnboundedSender<DeviceEvent>) {
    match device.list_save_states().await {
        Ok(states) => {
            let _ = events.send(DeviceEvent::SaveStatesLoaded(states));
        }
        Err(err) => {
            let _ = events.send(DeviceEvent::Error(format!("failed to list save states: {err}")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(key: &str) -> DeviceRequest {
        DeviceRequest::FetchImage { key: key.to_string(), image_path: format!("{key}.png") }
    }

    fn key_of(request: &DeviceRequest) -> Option<&str> {
        match request {
            DeviceRequest::FetchImage { key, .. } => Some(key),
            _ => None,
        }
    }

    #[test]
    fn only_the_newest_queued_image_fetch_survives() {
        let mut pending: VecDeque<DeviceRequest> = vec![fetch("a"), fetch("b"), fetch("c")].into();

        coalesce_image_fetches(&mut pending);

        assert_eq!(pending.len(), 1, "the rows already scrolled past are pure latency");
        assert_eq!(key_of(&pending[0]), Some("c"), "the newest is the only one that can be shown");
    }

    #[test]
    fn coalescing_never_drops_or_reorders_other_work() {
        let mut pending: VecDeque<DeviceRequest> = vec![
            fetch("a"),
            DeviceRequest::SyncFavorites(Vec::new()),
            fetch("b"),
            DeviceRequest::LoadSaveStates,
        ]
        .into();

        coalesce_image_fetches(&mut pending);

        // A dropped favourite sync is a lost write to the device, so only
        // image fetches may ever be discarded — and the survivors keep their
        // original order.
        assert_eq!(pending.len(), 3);
        assert!(matches!(pending[0], DeviceRequest::SyncFavorites(_)));
        assert_eq!(key_of(&pending[1]), Some("b"));
        assert!(matches!(pending[2], DeviceRequest::LoadSaveStates));
    }

    #[test]
    fn a_queue_with_no_image_fetches_is_left_alone() {
        let mut pending: VecDeque<DeviceRequest> =
            vec![DeviceRequest::Connect, DeviceRequest::LoadSaveStates].into();

        coalesce_image_fetches(&mut pending);

        assert_eq!(pending.len(), 2);
    }
}
