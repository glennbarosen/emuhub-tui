mod app;
mod device;
mod search;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use emuhub_core::cache::{self, Paths};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

use crate::app::{App, ConsoleKind, DetailItem, GameSettingsItem, Pane, SettingsItem};
use crate::device::{DeviceEvent, DeviceRequest};
use crate::search::SearchState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Checked before anything else touches the terminal or the config file:
    // `emuhub --version` used to fall straight into the `host_arg` branch
    // below and get persisted as the configured host, so a later launch
    // tried to connect to a device named "--version" and Settings > Change
    // IP showed that instead of an empty field.
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("emuhub {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!(
                "Usage: emuhub [HOST]\n\n\
                 HOST  Miyoo Mini+ IP address or hostname; persisted to config.toml.\n\
                       Omit to use the configured host, or set one later from Settings > Change IP."
            );
            return Ok(());
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!("emuhub starting");

    let paths = Paths::resolve()?;
    let mut config = cache::load_config(&paths)?;

    // `emuhub <host>` overrides (and persists) the configured host — handy
    // when the Miyoo's DHCP lease moves and re-typing the IP each launch
    // gets old fast.
    if let Some(host_arg) = std::env::args().nth(1) {
        if host_arg.starts_with('-') {
            eprintln!("emuhub: unrecognized option '{host_arg}'\nUsage: emuhub [HOST]");
            std::process::exit(2);
        }
        config.host = host_arg;
        cache::save_config(&paths, &config)?;
    }

    // Enter raw mode / the alternate screen *before* constructing `App` —
    // `App::new` queries the terminal for Kitty/Sixel graphics support
    // (`Picker::from_query_stdio`), which needs to read the escape-sequence
    // response synchronously; doing that query in cooked mode silently
    // fails and falls back to halfblocks. Every fallible non-terminal setup
    // step happens above this line so an early `?` never leaves the
    // terminal stuck in raw mode.
    let mut terminal = init_terminal()?;
    let mut app = App::new(config.host.clone());

    // Offline-first: paint from cache immediately, before the network is
    // even in the picture.
    let disk_cache = cache::load_cache(&paths);
    if !disk_cache.consoles.is_empty() {
        let grouped: Vec<(&'static str, Vec<_>)> = disk_cache
            .consoles
            .iter()
            .filter_map(|c| {
                emuhub_core::consoles::by_folder(&c.folder).map(|console| (console.folder, c.games.clone()))
            })
            .collect();
        app.load_library(grouped);
        // Through the setter, not a bare assignment: it also rebuilds the
        // Favourites row, which would otherwise stay empty until the device
        // answered — exactly when it's least likely to.
        app.load_cached_favorite_paths(disk_cache.favorites.into_iter().collect());
        // After the library, so the recent entries resolve against it.
        app.load_recents(disk_cache.recents);
        app.last_sync = disk_cache.last_full_sync;
        // Save states too, or an offline launch shows a full library in which
        // every game claims to have none — the listing itself only ever runs
        // on a successful connect.
        app.save_states = disk_cache.save_states;
        app.set_status("Loaded cached library — connecting...", false);
    }

    let (req_tx, req_rx) = mpsc::unbounded_channel::<DeviceRequest>();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<DeviceEvent>();

    let device_task = tokio::spawn(device::run(config.clone(), paths.clone(), req_rx, evt_tx));
    if !config.host.is_empty() {
        let _ = req_tx.send(DeviceRequest::Connect);
    } else {
        app.set_status("No host configured", true);
        app.open_ip_prompt();
    }

    let result = run_loop(&mut terminal, &mut app, &paths, &req_tx, &mut evt_rx).await;
    restore_terminal(&mut terminal)?;

    let _ = req_tx.send(DeviceRequest::Shutdown);
    let _ = device_task.await;

    // Persist whatever we ended the session with, so next launch starts
    // from a warm cache even if the device is now unreachable.
    persist_cache(&paths, &app);

    // Bound the image cache on the way out rather than at startup: every
    // fetch this session has already happened (so the LRU order is final) and
    // it costs the user nothing they can perceive. Purely housekeeping —
    // worth a log line, never a failed exit.
    match cache::prune_image_cache(&paths, config.image_cache_max_mb * 1024 * 1024) {
        Ok((0, _)) => {}
        Ok((removed, freed)) => tracing::info!(removed, freed, "pruned image cache"),
        Err(err) => tracing::warn!(%err, "failed to prune the image cache"),
    }

    result
}

fn init_terminal() -> anyhow::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn persist_cache(paths: &Paths, app: &App) {
    let consoles = app
        .consoles
        .iter()
        // Virtual rows (Recently Played) are views over games already cached
        // under their real console — persisting one would write a `__RECENT`
        // folder that `consoles::by_folder` can't resolve on next launch.
        .filter(|c| c.kind == ConsoleKind::Real && !c.games.is_empty())
        .map(|c| emuhub_core::models::CachedConsole {
            folder: c.folder.to_string(),
            name: c.name.to_string(),
            games: c.games.clone(),
        })
        .collect();
    let cache = emuhub_core::models::AppCache {
        consoles,
        favorites: app.favorite_paths.iter().cloned().collect(),
        recents: app.recents.clone(),
        save_states: app.save_states.clone(),
        last_full_sync: app.last_sync,
        version: 1,
    };
    if let Err(err) = cache::save_cache(paths, &cache) {
        tracing::warn!(%err, "failed to persist cache on exit");
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    paths: &Paths,
    req_tx: &mpsc::UnboundedSender<DeviceRequest>,
    evt_rx: &mut mpsc::UnboundedReceiver<DeviceEvent>,
) -> anyhow::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Never block the UI thread on the network — poll with a short
        // timeout (~30fps) and drain whatever the device task has sent.
        if event::poll(Duration::from_millis(33))? {
            let raw = event::read()?;
            tracing::trace!(?raw, "raw terminal event");
            if let Event::Key(key) = raw {
                if key.kind == KeyEventKind::Press {
                    handle_key(key.code, key.modifiers, app, paths, req_tx);
                }
            }
        }

        while let Ok(evt) = evt_rx.try_recv() {
            if let Some(entries) = app.apply_device_event(evt) {
                let _ = req_tx.send(DeviceRequest::SyncFavorites(entries));
            }
            maybe_request_image(app, req_tx);
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    app: &mut App,
    paths: &Paths,
    req_tx: &mpsc::UnboundedSender<DeviceRequest>,
) {
    tracing::trace!(?code, ?modifiers, "key event");

    // Ahead of every guard, including the destructive dialogs: with `q`
    // unbound in normal mode, ctrl-c is the only way out of the app, and an
    // exit that some modals swallow is not an exit. Nothing is confirmed on
    // the way out — quitting on a "delete?" prompt cancels it by not
    // answering.
    if code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }

    if let Some(prompt) = &mut app.ip_prompt {
        match code {
            KeyCode::Esc => app.ip_prompt = None,
            KeyCode::Enter => {
                if let Some(host) = app.confirm_ip_prompt() {
                    app.set_status(format!("Connecting to {host}..."), false);
                    let _ = req_tx.send(DeviceRequest::SetHost(host));
                }
            }
            KeyCode::Backspace => prompt.backspace(),
            KeyCode::Char(c) => prompt.push_char(c),
            _ => {}
        }
        return;
    }

    // Destructive dialogs ahead of every non-modal mode, so a pending
    // "are you sure?" can't have the keypress that answers it stolen by the
    // browser underneath. (Both open from normal mode, so neither can
    // co-occur with the IP prompt above.)
    if app.confirm_delete.is_some() {
        match code {
            // Only an explicit 'y' proceeds; every other key is a cancel,
            // including 'n', esc, and anything mistyped.
            KeyCode::Char('y') => {
                if let Some((game, plan)) = app.confirm_delete() {
                    app.set_status(format!("Deleting {}...", game.display_name()), false);
                    let _ = req_tx.send(DeviceRequest::DeleteGame { game, plan: Box::new(plan) });
                }
            }
            _ => app.confirm_delete = None,
        }
        return;
    }

    if let Some(prompt) = &mut app.rename_prompt {
        match code {
            KeyCode::Esc => app.rename_prompt = None,
            KeyCode::Enter => {
                if let Some((game, plan)) = app.confirm_rename_prompt() {
                    app.set_status(format!("Renaming {}...", game.display_name()), false);
                    let _ = req_tx.send(DeviceRequest::RenameGame { game, plan: Box::new(plan) });
                }
            }
            KeyCode::Backspace => prompt.backspace(),
            KeyCode::Char(c) => prompt.push_char(c),
            _ => {}
        }
        return;
    }

    // Below the destructive dialogs (a pending "are you sure?" keeps the
    // keyboard) but above every other overlay, since `?` can be opened over
    // any of them and must be dismissable without disturbing what's beneath.
    if app.help {
        // Any key closes: a reference card the user has to remember a key to
        // leave is a poor reference card.
        app.help = false;
        return;
    }

    // Above settings: this is what the settings menu opens into, and esc from
    // here should return to the browser rather than back into the menu.
    if let Some(discovery) = &mut app.discovery {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => app.discovery = None,
            KeyCode::Char('j') | KeyCode::Down => discovery.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => discovery.move_selection(-1),
            KeyCode::Enter => {
                if let Some(host) = app.confirm_discovery() {
                    app.set_status(format!("Connecting to {host}..."), false);
                    let _ = req_tx.send(DeviceRequest::SetHost(host));
                }
            }
            _ => {}
        }
        return;
    }

    if let Some(settings) = &mut app.settings {
        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') | KeyCode::Left => {
                app.settings = None;
            }
            KeyCode::Char('j') | KeyCode::Down => settings.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => settings.move_selection(-1),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => match app.confirm_settings() {
                Some(SettingsItem::Reconnect) => {
                    app.set_status("Reconnecting...", false);
                    let _ = req_tx.send(DeviceRequest::Connect);
                }
                // Replaces the menu rather than stacking on top of it: esc
                // from the prompt returns to the browser, not to settings.
                Some(SettingsItem::ChangeIp) => app.open_ip_prompt(),
                Some(SettingsItem::FindDevice) => {
                    app.open_discovery();
                    app.set_status("Scanning the local network...", false);
                    let _ = req_tx.send(DeviceRequest::Discover);
                }
                None => {}
            },
            _ => {}
        }
        return;
    }

    // Above search so its own j/k/esc win while it's open, below the IP
    // prompt and settings so those still stack over it.
    if let Some(saves) = &mut app.saves {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => app.saves = None,
            KeyCode::Char('j') | KeyCode::Down => saves.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => saves.move_selection(-1),
            _ => {}
        }
        // Repaints the thumbnail for whatever slot is now selected — or, if
        // the browser just closed, refetches the game's box art.
        maybe_request_image(app, req_tx);
        return;
    }

    if let Some(search) = &mut app.search {
        match code {
            KeyCode::Esc => app.search = None,
            KeyCode::Enter => {
                if let Some(m) = search.matches.get(search.selected) {
                    app.console_idx = m.console_idx;
                    app.game_idx = m.game_idx;
                    app.focus = Pane::Games;
                    app.search = None;
                    app.reset_detail_menu();
                }
            }
            KeyCode::Up => app.move_selection(-1),
            KeyCode::Down => app.move_selection(1),
            KeyCode::Backspace => {
                let consoles = &app.consoles;
                search.backspace(consoles);
            }
            KeyCode::Char(c) => {
                let consoles = &app.consoles;
                search.push_char(c, consoles);
            }
            _ => {}
        }
        maybe_request_image(app, req_tx);
        return;
    }

    // No single-key quit: `q` still closes the overlays above, but in the
    // browser it does nothing — a mistyped key while scrolling a library
    // shouldn't end the session. Ctrl-c is handled at the top of this
    // function, so it works here too.
    match code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.move_selection(1);
            maybe_request_image(app, req_tx);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.move_selection(-1);
            maybe_request_image(app, req_tx);
        }
        // In the Details pane, `h`/`esc` backs out of the nested settings
        // level first and only then leaves the pane — one "back" per level.
        KeyCode::Char('h') | KeyCode::Left | KeyCode::Esc => {
            if app.focus == Pane::Detail && app.detail.back() {
                return;
            }
            app.focus_prev();
        }
        KeyCode::Char('l') | KeyCode::Right | KeyCode::Enter => {
            match app.focus {
                // Consoles → Games → Detail: `enter` on a game drills into
                // the Details pane and turns its action list live.
                Pane::Consoles | Pane::Games => {
                    if app.selected_game().is_some() || app.focus == Pane::Consoles {
                        app.focus_next();
                    }
                }
                Pane::Detail => activate_detail_item(app, paths, req_tx),
            }
            maybe_request_image(app, req_tx);
        }
        KeyCode::Char('g') => {
            match app.focus {
                Pane::Consoles => {
                    if let Some(idx) = app.consoles.iter().position(|c| !c.games.is_empty()) {
                        app.console_idx = idx;
                        app.reset_detail_menu();
                    }
                }
                Pane::Games => {
                    app.game_idx = 0;
                    app.reset_detail_menu();
                }
                Pane::Detail => app.detail.selected = 0,
            }
            maybe_request_image(app, req_tx);
        }
        KeyCode::Char('G') => {
            match app.focus {
                Pane::Consoles => {
                    if let Some(idx) = app.consoles.iter().rposition(|c| !c.games.is_empty()) {
                        app.console_idx = idx;
                        app.reset_detail_menu();
                    }
                }
                Pane::Games => {
                    if let Some(c) = app.current_console() {
                        app.game_idx = c.games.len().saturating_sub(1);
                    }
                    app.reset_detail_menu();
                }
                Pane::Detail => app.detail.selected = app.detail.len().saturating_sub(1),
            }
            maybe_request_image(app, req_tx);
        }
        KeyCode::Char('/') => {
            app.search = Some(SearchState::new());
        }
        KeyCode::Char('s') => {
            app.open_settings();
        }
        // Re-scan over the live session. Reconnecting (s → Reconnect) also
        // refreshes, but it drops a working SSH session to do it.
        KeyCode::Char('r') => {
            app.set_status("Refreshing library...", false);
            let _ = req_tx.send(DeviceRequest::RefreshLibrary);
        }
        KeyCode::Char('?') => {
            app.help = true;
        }
        // Saves, favourite, rename and delete are deliberately *not* bound
        // here. They are per-game actions and the Details pane menu (`enter`)
        // is their only entry point, so there's one discoverable route to each
        // instead of a documented menu plus a set of hidden shortcuts that
        // have to be kept in sync with it.
        _ => {}
    }
}

/// Runs the action the Details pane's cursor is on: the top level either opens
/// the saves browser or descends into the per-game settings, and the nested
/// level performs the rename/favourite/delete actions.
fn activate_detail_item(app: &mut App, paths: &Paths, req_tx: &mpsc::UnboundedSender<DeviceRequest>) {
    if let Some(item) = app.confirm_detail() {
        match item {
            DetailItem::ShowSaves => open_saves_browser(app, req_tx),
            // `confirm_detail` already switched the level; nothing else to do.
            DetailItem::Settings => {}
        }
        return;
    }

    // Each arm reports whether it had a game to act on, so the "nothing
    // selected" message is written once rather than per action.
    let acted = match app.confirm_game_setting() {
        Some(GameSettingsItem::Rename) => app.open_rename_prompt(),
        Some(GameSettingsItem::ToggleFavorite) => {
            toggle_favorite_and_persist(app, paths, req_tx);
            true
        }
        Some(GameSettingsItem::Delete) => app.open_confirm_delete(),
        None => true,
    };
    if !acted {
        app.set_status("No game selected", true);
    }
}

/// Opens the save-state browser for the selected game, asking the device for a
/// listing first when we don't have one.
///
/// That refresh is the recovery path for a session that connected while the
/// handheld was asleep: the listing used to run only inside `do_connect`, so
/// every game reported "no save states" for the rest of the session with
/// nothing to retry. `SaveStatesLoaded` refreshes an open browser in place, so
/// the list fills itself in when the reply lands.
fn open_saves_browser(app: &mut App, req_tx: &mpsc::UnboundedSender<DeviceRequest>) {
    if !app.open_saves() {
        app.set_status("No game selected", true);
        return;
    }
    if app.needs_save_listing() {
        let _ = req_tx.send(DeviceRequest::LoadSaveStates);
    }
    maybe_request_image(app, req_tx);
}

/// Flips the selected game's favourite status and pushes it to the device.
///
/// The local cache write is immediate and deliberate: a toggle made offline
/// has to survive a crash or quit before the next full cache write on exit.
fn toggle_favorite_and_persist(app: &mut App, paths: &Paths, req_tx: &mpsc::UnboundedSender<DeviceRequest>) {
    let Some(game) = app.selected_game().cloned() else {
        return;
    };
    let entries = app.toggle_favorite(&game);
    let _ = req_tx.send(DeviceRequest::SyncFavorites(entries));
    let mut cache = cache::load_cache(paths);
    cache.favorites = app.favorite_paths.iter().cloned().collect();
    let _ = cache::save_cache(paths, &cache);
}

/// Requests whatever image the UI wants on screen — the selected game's box
/// art, or the selected save state's thumbnail while the saves browser is
/// open (`App::current_image` decides which) — unless it's already the one
/// decoded in the single-slot `image_state`.
///
/// Re-requesting a previously-seen image is intentional and cheap — the
/// device task's disk cache serves it back near-instantly — because the
/// alternative (an "already requested this session" gate) is what caused box
/// art to get permanently stuck missing for any game visited before the most
/// recent one: `image_state` holds only the last decoded image, so navigating
/// away evicts it, and a one-time-only gate would then refuse to ever
/// re-fetch it.
fn maybe_request_image(app: &mut App, req_tx: &mpsc::UnboundedSender<DeviceRequest>) {
    let Some((key, image_path)) = app.current_image() else {
        return;
    };
    if app.image_for_key.as_deref() == Some(key.as_str()) {
        return;
    }
    let _ = req_tx.send(DeviceRequest::FetchImage { key, image_path });
}
