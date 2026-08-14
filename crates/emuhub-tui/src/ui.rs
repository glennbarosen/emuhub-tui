//! Pure rendering — a function of `&App`, no I/O, no state mutation.
//! Status line on top, consoles/games side by side below it, a full-width
//! Details pane (box art left, text right) under those, footer keybindings
//! at the bottom. Search renders as a popup over the games pane rather than
//! a separate pane; the settings menu and IP prompt float over everything.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use ratatui_image::StatefulImage;

use emuhub_core::models::GameFile;
use emuhub_core::{cache, consoles};

use crate::app::{
    App, ConnectionState, DetailItem, DetailLevel, GameSettingsItem, Pane, SettingsItem, HELP_FOOTNOTE,
    HELP_SECTIONS,
};

pub fn draw(frame: &mut Frame, app: &mut App) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());

    draw_status_line(frame, app, root[0]);

    let content = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(14)])
        .split(root[1]);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(20)])
        .split(content[0]);

    draw_consoles(frame, app, columns[0]);
    if app.search.is_some() {
        draw_search(frame, app, columns[1]);
    } else {
        draw_games(frame, app, columns[1]);
    }
    draw_detail(frame, app, content[1]);

    draw_footer(frame, app, root[2]);

    // Saves first, then settings, IP prompt last: the prompt is what settings
    // opens into, so it must win if both are ever set at once.
    if app.saves.is_some() {
        draw_saves(frame, app, frame.area());
    }
    if app.settings.is_some() {
        draw_settings(frame, app);
    }
    if app.discovery.is_some() {
        draw_discovery(frame, app);
    }
    if app.ip_prompt.is_some() {
        draw_ip_prompt(frame, app);
    }
    // Above the other overlays (it can be opened over any of them) but below
    // the destructive dialogs, matching the guard order in `handle_key`.
    draw_help(frame, app);
    // Destructive dialogs draw last so nothing can obscure the thing the user
    // is being asked to confirm.
    if app.rename_prompt.is_some() {
        draw_rename_prompt(frame, app);
    }
    if app.confirm_delete.is_some() {
        draw_confirm_delete(frame, app);
    }
}

fn draw_status_line(frame: &mut Frame, app: &App, area: Rect) {
    // How stale the cached library is, but only where it changes what the
    // user should believe: while offline or not yet connected, the lists on
    // screen are the cache, and "when was this true?" is the missing fact.
    // Online it's live by definition, so the age would be noise.
    let age = app
        .last_sync
        .map(|then| cache::relative_age(then, cache::now_epoch()))
        .unwrap_or_else(|| "never synced".to_string());

    let (dot, label, color) = match &app.connection {
        ConnectionState::Disconnected => ("○", format!("disconnected · {age}"), Color::DarkGray),
        ConnectionState::Connecting => ("◐", "connecting...".to_string(), Color::Yellow),
        ConnectionState::Connected => ("●", "online".to_string(), Color::Green),
        ConnectionState::Offline(_) => ("●", format!("offline (cache · {age})"), Color::Red),
    };
    let host = if app.host.is_empty() { "no host configured".to_string() } else { app.host.clone() };
    let mut spans = vec![
        Span::styled(" emuhub ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("─ "),
        Span::raw(host),
        Span::raw(" · "),
        Span::styled(dot, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(color)),
    ];
    if let Some(status) = &app.status {
        let color = if status.is_error { Color::Red } else { Color::Green };
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(status.text.clone(), Style::default().fg(color)));
    }
    if app.has_pending_changes() {
        let count = app.pending_additions.len() + app.pending_removals.len();
        spans.push(Span::raw(" · "));
        spans.push(Span::styled(
            format!("{count} favourite(s) pending sync"),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Note the two different indices in play: `app.console_idx` indexes
/// `app.consoles`, but consoles with no ROMs are hidden, so the *row* a
/// console occupies here is its position among the non-empty ones. The list
/// state (and therefore the scroll offset) must be given the row, not the
/// console index, or long lists scroll to the wrong place.
fn draw_consoles(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Consoles && app.search.is_none();
    let visible = app.visible_console_indices();
    let selected_row = app.selected_console_row();

    let items: Vec<ListItem> = visible
        .iter()
        .map(|&i| {
            let c = &app.consoles[i];
            let selected = i == app.console_idx;
            let marker = if selected && focused { "▸ " } else { "  " };
            let text = format!("{marker}{} {}  {}", c.icon, c.name, c.games.len());
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD).fg(if focused {
                    Color::Cyan
                } else {
                    Color::White
                })
            } else {
                Style::default()
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let block =
        Block::default().borders(Borders::ALL).title(" Consoles ").border_style(border_style(focused));
    let state = &mut app.console_list;
    state.select(selected_row);
    frame.render_stateful_widget(List::new(items).block(block), area, state);
}

fn draw_games(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Games;
    let console = app.current_console();
    let title =
        console.map(|c| format!(" {} · {} roms ", c.name, c.games.len())).unwrap_or_else(|| " Games ".into());
    let has_games = console.map(|c| !c.games.is_empty()).unwrap_or(false);

    let items: Vec<ListItem> = console
        .map(|c| {
            c.games
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let star = if app.is_favorite(g) { "★ " } else { "  " };
                    let selected = i == app.game_idx;
                    let marker = if selected && focused { "▸ " } else { "  " };
                    // Save count is free to show — the whole save tree came
                    // down in one listing on connect.
                    let saves = match app.save_count(g) {
                        0 => String::new(),
                        n => format!("  ⦿{n}"),
                    };
                    let text = format!("{marker}{star}{}{saves}", g.display_name());
                    let style = if selected {
                        Style::default().add_modifier(Modifier::BOLD).fg(if focused {
                            Color::Cyan
                        } else {
                            Color::White
                        })
                    } else {
                        Style::default()
                    };
                    ListItem::new(text).style(style)
                })
                .collect()
        })
        .unwrap_or_default();

    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style(focused));
    let selected_row = has_games.then_some(app.game_idx);
    let state = &mut app.games_list;
    state.select(selected_row);
    frame.render_stateful_widget(List::new(items).block(block), area, state);
}

fn draw_search(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(search) = &app.search else { return };
    let items: Vec<ListItem> = search
        .matches
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let console = &app.consoles[m.console_idx];
            let game = &console.games[m.game_idx];
            let star = if app.is_favorite(game) { "★ " } else { "  " };
            let marker = if i == search.selected { "▸ " } else { "  " };
            let text = format!("{marker}{star}[{}] {}", console.folder, game.display_name());
            let style = if i == search.selected {
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(text).style(style)
        })
        .collect();

    let title = format!(
        " Search: {}_  ({} match{}) ",
        search.query,
        search.matches.len(),
        if search.matches.len() == 1 { "" } else { "es" }
    );
    let selected_row = (!search.matches.is_empty()).then_some(search.selected);
    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style(true));
    let state = &mut app.search_list;
    state.select(selected_row);
    frame.render_stateful_widget(List::new(items).block(block), area, state);
}

/// The settings menu — like `draw_ip_prompt`, a floating dialog rather than a
/// pane, since it must work before any library has loaded. Rows come straight
/// off `SettingsItem::ALL`, so adding a setting needs no change here.
fn draw_settings(frame: &mut Frame, app: &App) {
    let Some(settings) = &app.settings else {
        return;
    };
    let area = centered_rect(50, 30, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default().borders(Borders::ALL).title(" Settings ").border_style(border_style(true));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = SettingsItem::ALL
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let selected = i == settings.selected;
            let marker = if selected { "▸ " } else { "  " };
            let line = match item {
                // Doubles as a readout of what we're currently pointed at.
                SettingsItem::ChangeIp => {
                    let host = if app.host.is_empty() { "not set" } else { app.host.as_str() };
                    Line::from(vec![
                        Span::raw(format!("{marker}{}  ", item.label())),
                        Span::styled(host, Style::default().fg(Color::DarkGray)),
                    ])
                }
                _ => Line::from(format!("{marker}{}", item.label())),
            };
            let style = if selected {
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();
    frame.render_widget(List::new(items), rows[0]);

    let hint = Paragraph::new(Span::styled(
        "↑↓ move · enter select · esc close",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint, rows[1]);
}

/// The save-state browser: slot list on the left, that slot's actual
/// screenshot on the right. The thumbnail goes through the same single image
/// slot as box art (see `App::current_image`), so exactly one of the two is
/// ever decoded at a time.
fn draw_saves(frame: &mut Frame, app: &mut App, whole: Rect) {
    let Some(saves) = &app.saves else { return };
    let area = centered_rect(70, 60, whole);
    frame.render_widget(Clear, area);

    let title = format!(" Save states · {} ", saves.game.display_name());
    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style(true));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);

    // An empty browser has to say *which* kind of empty it is. "No save states
    // for this game" on a card whose listing never arrived (device asleep at
    // connect, or the listing errored) sent every previous investigation down
    // the wrong path.
    if saves.states.is_empty() {
        let text = if app.save_states.is_empty() {
            match &app.connection {
                ConnectionState::Connected => vec![
                    Line::from("No save states found on the device."),
                    Line::from(Span::styled(
                        "Nothing under Saves/CurrentProfile/{states,saves}.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
                _ => vec![
                    Line::from("Device offline — save listing not loaded."),
                    Line::from(Span::styled(
                        "Reconnect (s) to load save states.",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
            }
        } else {
            vec![
                Line::from("No save states for this game."),
                Line::from(Span::styled(
                    format!("{} save(s) on the card belong to other games.", app.save_states.len()),
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), rows[0]);
        let hint = Paragraph::new(Span::styled("esc close", Style::default().fg(Color::DarkGray)));
        frame.render_widget(hint, rows[1]);
        return;
    }

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Min(10)])
        .split(rows[0]);

    let items: Vec<ListItem> = saves
        .states
        .iter()
        .enumerate()
        .map(|(i, state)| {
            let marker = if i == saves.selected { "▸ " } else { "  " };
            let thumb = if state.thumbnail_path.is_some() { "" } else { "  (no preview)" };
            let line = Line::from(vec![
                Span::raw(format!("{marker}{}", state.display_name())),
                Span::styled(thumb, Style::default().fg(Color::DarkGray)),
            ]);
            let style = if i == saves.selected {
                Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
            } else {
                Style::default()
            };
            ListItem::new(line).style(style)
        })
        .collect();
    let selected_row = (!saves.states.is_empty()).then_some(saves.selected);

    // The preview key has to be read before the mutable borrow for the list
    // state below.
    let preview = saves.current().and_then(|s| s.thumbnail_path.clone());

    let state = &mut app.saves.as_mut().expect("checked above").list;
    state.select(selected_row);
    frame.render_stateful_widget(List::new(items), cols[0], state);

    match preview {
        Some(key) if app.image_for_key.as_deref() == Some(key.as_str()) => {
            if let Some(protocol) = app.image_state.as_mut() {
                frame.render_stateful_widget(StatefulImage::default(), cols[1], protocol);
            }
        }
        Some(_) => {
            let loading = Paragraph::new("(loading preview...)").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(loading, cols[1]);
        }
        None => {
            let none =
                Paragraph::new("(this slot has no screenshot)").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(none, cols[1]);
        }
    }

    let hint = Paragraph::new(Span::styled("↑↓ move · esc close", Style::default().fg(Color::DarkGray)));
    frame.render_widget(hint, rows[1]);
}

/// The delete confirmation — and the reason the cascade is computed as data
/// before any I/O happens: this lists the exact files that are about to be
/// removed, so the dialog *is* the dry run.
fn draw_confirm_delete(frame: &mut Frame, app: &App) {
    let Some(confirm) = &app.confirm_delete else {
        return;
    };
    let area = centered_rect(75, 65, frame.area());
    frame.render_widget(Clear, area);

    let title = format!(" Delete {} ? ", confirm.game.display_name());
    let block =
        Block::default().borders(Borders::ALL).title(title).border_style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let summary = confirm.plan.summary();
    let warning = Paragraph::new(Line::from(vec![
        Span::styled("This cannot be undone. ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::raw(format!("{} change(s) on the device:", summary.len())),
    ]));
    frame.render_widget(warning, rows[0]);

    let lines: Vec<Line> = summary
        .iter()
        .map(|line| {
            let color = if line.starts_with("delete") { Color::Red } else { Color::Yellow };
            Line::from(Span::styled(line.clone(), Style::default().fg(color)))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rows[1]);

    let hint = Paragraph::new(Span::styled(
        "y delete · any other key cancels",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint, rows[2]);
}

/// The rename prompt, with the same up-front dry run the delete dialog shows.
///
/// A rename is a multi-file cascade too — box art, save states and their
/// thumbnails, both NDJSON lists, and the console's `cache6.db` — and listing
/// them only for the irreversible action left the recoverable one looking like
/// it touched a single file.
fn draw_rename_prompt(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.rename_prompt else {
        return;
    };
    let area = centered_rect(60, 55, frame.area());
    frame.render_widget(Clear, area);

    let block =
        Block::default().borders(Borders::ALL).title(" Rename game ").border_style(border_style(true));
    let mut text = vec![
        Line::from(Span::styled(format!("{} → ", prompt.game.name), Style::default().fg(Color::DarkGray))),
        Line::from(format!("{}_", prompt.input)),
        Line::from(""),
    ];
    if let Some(error) = &prompt.error {
        text.push(Line::from(Span::styled(error.clone(), Style::default().fg(Color::Red))));
    } else {
        // Renaming the extension would change which emulator the file belongs
        // to, so it's fixed and the user should know that up front.
        text.push(Line::from(Span::styled(
            format!("keeps the .{} extension", prompt.game.extension),
            Style::default().fg(Color::DarkGray),
        )));
    }

    if let Some(summary) = app.rename_preview() {
        text.push(Line::from(""));
        text.push(Line::from(Span::raw(format!("{} change(s) on the device:", summary.len()))));
        text.extend(
            summary
                .into_iter()
                .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::Yellow)))),
        );
    }

    text.push(Line::from(""));
    text.push(Line::from(Span::styled("enter confirm · esc cancel", Style::default().fg(Color::DarkGray))));
    frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), area);
}

/// The `?` overlay, rendered straight off `HELP_SECTIONS` with the key column
/// aligned to the widest binding — the same table-driven approach as the
/// settings menu, so adding a binding never touches this function.
fn draw_help(frame: &mut Frame, app: &App) {
    if !app.help {
        return;
    }
    let area = centered_rect(64, 80, frame.area());
    frame.render_widget(Clear, area);

    let width = HELP_SECTIONS
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);

    let mut text: Vec<Line> = Vec::new();
    for (section, rows) in HELP_SECTIONS {
        if !text.is_empty() {
            text.push(Line::from(""));
        }
        text.push(Line::from(Span::styled(
            *section,
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
        )));
        for (keys, meaning) in *rows {
            text.push(Line::from(vec![
                Span::styled(
                    format!("  {keys:<width$}  "),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                ),
                Span::raw(*meaning),
            ]));
        }
    }
    text.push(Line::from(""));
    text.push(Line::from(Span::styled(HELP_FOOTNOTE, Style::default().fg(Color::DarkGray))));

    let block = Block::default().borders(Borders::ALL).title(" Keys ").border_style(border_style(true));
    frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), area);
}

/// Device discovery: a progress line while the /24 sweep runs, and a pick
/// list underneath. Results are selectable as they land, so a device found
/// early doesn't have to wait for the rest of the sweep.
fn draw_discovery(frame: &mut Frame, app: &mut App) {
    let Some(discovery) = &app.discovery else {
        return;
    };
    let area = centered_rect(55, 45, frame.area());
    frame.render_widget(Clear, area);

    let block =
        Block::default().borders(Borders::ALL).title(" Find device ").border_style(border_style(true));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    // Always name the range being swept. A scan that finds nothing is
    // otherwise indistinguishable from a scan that looked at the wrong
    // addresses — which is exactly how a /22 network hid the handheld.
    let range = discovery.network.as_deref().unwrap_or("local network");
    let progress = if discovery.finished {
        format!("Scan of {range} complete — {} device(s) found", discovery.found.len())
    } else if discovery.total == 0 {
        format!("Scanning {range}...")
    } else {
        format!("Scanning {range}... {}/{} hosts probed", discovery.done, discovery.total)
    };
    let color = if discovery.finished { Color::Green } else { Color::Yellow };
    frame.render_widget(
        Paragraph::new(Span::styled(progress, Style::default().fg(color))).wrap(Wrap { trim: true }),
        rows[0],
    );

    if discovery.found.is_empty() {
        // With nothing found, the hosts that *did* answer on :22 are the
        // most useful thing on screen — one of them may be the handheld
        // refusing our credentials rather than a NAS.
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            if discovery.finished { "No handheld found." } else { "(nothing found yet)" },
            Style::default().fg(Color::DarkGray),
        ))];
        for (host, reason) in &discovery.rejected {
            lines.push(Line::from(Span::styled(
                format!("{host} answered on :22 — {reason}"),
                Style::default().fg(Color::Yellow),
            )));
        }
        if discovery.finished && discovery.rejected.is_empty() {
            lines.push(Line::from(Span::styled(
                "Nothing answered on port 22. Wake the handheld, check it's on the same wifi, then rescan.",
                Style::default().fg(Color::DarkGray),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), rows[1]);
    } else {
        let items: Vec<ListItem> = discovery
            .found
            .iter()
            .enumerate()
            .map(|(i, host)| {
                let marker = if i == discovery.selected { "▸ " } else { "  " };
                let style = if i == discovery.selected {
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
                } else {
                    Style::default()
                };
                ListItem::new(format!("{marker}{host}")).style(style)
            })
            .collect();
        let selected_row = Some(discovery.selected);
        let state = &mut app.discovery.as_mut().expect("checked above").list;
        state.select(selected_row);
        frame.render_stateful_widget(List::new(items), rows[1], state);
    }

    let hint = Paragraph::new(Span::styled(
        "↑↓ move · enter use · esc cancel",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(hint, rows[2]);
}

/// A blocking dialog rather than an in-place list filter (unlike `search`,
/// which replaces the games pane) — it must work even with no library
/// loaded yet (first launch), so it floats over the whole screen instead of
/// tying itself to a pane's `Rect`.
fn draw_ip_prompt(frame: &mut Frame, app: &App) {
    let Some(prompt) = &app.ip_prompt else { return };
    let area = centered_rect(50, 20, frame.area());
    frame.render_widget(Clear, area);

    let title = if app.host.is_empty() { " Set Miyoo IP address " } else { " Change Miyoo IP address " };
    let block = Block::default().borders(Borders::ALL).title(title).border_style(border_style(true));
    let text = vec![
        Line::from(format!("{}_", prompt.input)),
        Line::from(""),
        Line::from(Span::styled("enter confirm · esc cancel", Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(Paragraph::new(text).block(block), area);
}

/// Standard ratatui nested-layout idiom for a centered floating popup.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Box art, metadata, and the per-game action list. The actions are always
/// drawn — dimmed until `enter` focuses this pane — which is the whole point:
/// rename/favourite/delete used to be unadvertised single-key bindings.
fn draw_detail(frame: &mut Frame, app: &mut App, area: Rect) {
    let focused = app.focus == Pane::Detail;
    let block = Block::default().borders(Borders::ALL).title(" Details ").border_style(border_style(focused));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(game) = app.selected_game().cloned() else {
        frame.render_widget(Paragraph::new("No game selected").wrap(Wrap { trim: true }), inner);
        return;
    };

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(28), Constraint::Min(20), Constraint::Length(24)])
        .split(inner);

    draw_detail_actions(frame, app, &game, cols[2], focused);

    // Box art, if we have one decoded for this exact game.
    if app.image_for_key.as_deref() == Some(game.path.as_str()) {
        if let Some(protocol) = app.image_state.as_mut() {
            frame.render_stateful_widget(StatefulImage::default(), cols[0], protocol);
        }
    } else {
        let placeholder = Paragraph::new("(no box art loaded)").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(placeholder, cols[0]);
    }

    let star = if app.is_favorite(&game) { "★ favourite" } else { "not favourited" };
    let pending = if app.pending_additions.contains(&game.path) || app.pending_removals.contains(&game.path) {
        " (pending sync)"
    } else {
        ""
    };

    let mut text = vec![
        Line::from(Span::styled(game.display_name(), Style::default().add_modifier(Modifier::BOLD))),
        Line::from(format!("console  {}", game.console_folder)),
    ];

    // The RetroArch core matters here because it's *why* save states live
    // where they do (Quirk 3) — a game whose saves seem missing is usually a
    // game whose console has no core mapping.
    let cores = consoles::core_names_for(&game.console_folder);
    if !cores.is_empty() {
        text.push(Line::from(format!("core     {}", cores.join(", "))));
    }

    text.push(Line::from(format!("path     {}", game.path)));

    // Size is absent when the card's busybox couldn't give us one (see
    // `Device::list_all_roms`); the row is dropped rather than showing "?".
    if let Some(size) = game.size {
        text.push(Line::from(format!("size     {}", format_size(size))));
    }

    let states = app.states_for(&game);
    if !states.is_empty() {
        let slots: Vec<String> = states.iter().map(|s| s.display_name()).collect();
        text.push(Line::from(format!("saves    {}", slots.join(", "))));
    }

    // Position in the play history, not a date: `recentlist.json` entries
    // carry no timestamp, so rank is the only recency the device gives us.
    if let Some(rank) = app.recent_rank(&game) {
        text.push(Line::from(format!("recent   #{rank} most recently played")));
    }

    text.push(Line::from(format!("{star}{pending}")));
    frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), cols[1]);
}

/// Byte count as something a person reads at a glance. ROMs run from a few KB
/// (NES) to a few GB (PS1 discs), so all three units earn their place.
fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The Details pane's action column. Driven entirely off `DetailItem::ALL` /
/// `GameSettingsItem::ALL`, the same way `draw_settings` runs off
/// `SettingsItem::ALL` — adding an action is one variant and one dispatch arm,
/// with no change here.
fn draw_detail_actions(frame: &mut Frame, app: &App, game: &GameFile, area: Rect, focused: bool) {
    let is_favorite = app.is_favorite(game);
    let (heading, labels): (&str, Vec<String>) = match app.detail.level {
        DetailLevel::Actions => (
            "ACTIONS",
            DetailItem::ALL
                .iter()
                .map(|item| match item {
                    // The count is the same one the games list shows as ⦿N,
                    // so an empty browser is never a surprise.
                    DetailItem::ShowSaves => match app.save_count(game) {
                        0 => item.label().to_string(),
                        n => format!("{}  ⦿{n}", item.label()),
                    },
                    DetailItem::Settings => item.label().to_string(),
                })
                .collect(),
        ),
        DetailLevel::Settings => (
            "SETTINGS",
            GameSettingsItem::ALL.iter().map(|item| item.label(is_favorite).to_string()).collect(),
        ),
    };

    let mut lines = vec![Line::from(Span::styled(
        heading,
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD),
    ))];

    lines.extend(labels.iter().enumerate().map(|(i, label)| {
        let selected = i == app.detail.selected;
        let marker = if selected && focused { "▸ " } else { "  " };
        // Unfocused, the whole column reads as an affordance rather than a
        // live menu — visible, obviously inert.
        let style = if !focused {
            Style::default().fg(Color::DarkGray)
        } else if selected {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(Span::styled(format!("{marker}{label}"), style))
    }));

    let hint = if focused { "enter select · esc back" } else { "enter for actions" };
    lines.push(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))));

    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let line = if app.help {
        Line::from(" any key closes ")
    } else if app.confirm_delete.is_some() {
        Line::from(" y delete · any other key cancels ")
    } else if app.rename_prompt.is_some() {
        Line::from(" type a new name · enter confirm · esc cancel ")
    } else if app.ip_prompt.is_some() {
        Line::from(" type IP address · enter confirm · esc cancel ")
    } else if app.discovery.is_some() {
        Line::from(" ↑↓ move · enter use · esc cancel ")
    } else if app.settings.is_some() {
        Line::from(" ↑↓ move · enter select · esc close ")
    } else if app.saves.is_some() {
        Line::from(" ↑↓ move · esc close ")
    } else if app.search.is_some() {
        // Every printable key goes into the query here, so `f` types an "f"
        // rather than favouriting — don't advertise it.
        Line::from(" type to search · ↑↓ navigate · enter jump · esc cancel ")
    } else if app.focus == Pane::Detail {
        Line::from(" ↑↓ move · enter select · esc back ")
    } else {
        // Per-game actions deliberately aren't listed: they live in the
        // Details pane menu, which is what `enter` opens — `?` is where that
        // gets explained.
        Line::from(" enter details · / search · r refresh · s settings · ? help · ctrl-c quit ")
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}
