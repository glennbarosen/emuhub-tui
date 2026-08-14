# AGENTS.md — emuhub-tui

Rust TUI rewrite of `glennbarosen/emuhub` (SwiftUI macOS/iOS), for browsing ROMs and syncing favourites
on a **Miyoo Mini+ / Onion OS** handheld over SSH/SFTP. The full device-protocol writeup — where Onion
OS keeps things and the file-format quirks that bite — lives in `docs/DEVICE-PROTOCOL.md`; read that for
*why*. This file is for *how to build and work in this repo* day to day. `README.md` is the front door
for users rather than contributors.

## Skills

Three recurring tasks have skills in `.claude/skills/` — prefer them over reinventing the workflow:
`run-against-device` (test a change on the real handheld), `add-console` (register a new
console/system), `install-emuhub` (build/update the `emuhub` binary on `PATH`).

## Stack

Rust (2021 edition), Tokio, `russh`/`russh-sftp` (in-process SSH/SFTP, no shelling out), `ratatui` +
`crossterm` (TUI), `ratatui-image` (Kitty graphics protocol box art, halfblock fallback), `nucleo`
(fuzzy search), `serde`/`serde_json`/`toml`, `directories` (XDG paths).

## Architecture map

Two-crate workspace so a headless CLI mode is nearly free later:

- `crates/emuhub-core/` — models, device-protocol quirks, SSH/SFTP transport, XDG cache. **No TUI
  deps.** Fully unit-testable without hardware (72 tests, fixtures in `tests/fixtures/`).
  - `models.rs` — `Console`, `GameFile`, `FavoriteGame`, `PlayHistoryEntry`, `SaveState`, `AppCache`.
  - `path.rs` — Quirk 2: resolves `../../` segments the device writes into favourite/recent rompaths.
  - `consoles.rs` — static console list, ROM extensions, and Quirk 3's console→RetroArch-core table
    (needed to find save states, filed by core name not console folder). Copied verbatim from the
    original — don't re-derive it.
  - `favorites.rs` — Quirk 1: `favourite.json`/`recentlist.json` are **NDJSON** (one object per line,
    no array wrapper). Writing a JSON array silently breaks favourites on-device.
  - `scan.rs` — parses one `find` over `/mnt/SDCARD/Roms` (one exec round trip for the whole library,
    replacing the Swift original's 25-round-trip per-console `preloadData()` bug). Accepts both the
    bare-path and `stat -c '%s %n'` line shapes, which is what lets `list_all_roms` ask for file sizes
    and fall back to a plain `find` on a busybox that can't. `parse_rom_path` is also how a
    favourite/recent `rompath` is turned back into a `GameFile`.
  - `saves.rs` — Quirk 3 in practice: parses one `find` over `states/` + `saves/` into `SaveState`s,
    pairs each with its `{statefile}.png` thumbnail, and matches them to a ROM by basename
    (`states_for_game`). Same one-round-trip approach as `scan.rs`.
  - `cascade.rs` — what deleting/renaming a ROM *actually* has to touch: box art, saves and their
    thumbnails, the `favourite.json` and `recentlist.json` entries, and Onion's `{CONSOLE}_cache6.db`.
    Pure plans, no I/O — which is what lets the confirm dialog show the exact file list up front.
  - `discover.rs` — TCP:22 sweep of the local /24, then a real connect + `/mnt/SDCARD` check to
    confirm a hit is the handheld and not just something with SSH open.
  - `cache.rs` — XDG paths: config `~/.config/emuhub/`, library/favourites cache `~/.local/state/emuhub/`,
    box-art cache `~/.cache/emuhub/images/`.
  - `transport.rs` — `Device::connect` (the dropbear-compatible legacy SSH algorithms, see Gotchas),
    `exec` (pub — also usable for ad-hoc device debugging / a future `emuhub ls GBA` CLI), and the
    write path: `write_favorites`/`write_recents` (both `.bak` first), `remove_file`/`rename_file`
    (a missing target is success, since a cascade plans for art/saves that may not exist), and
    `apply_delete`/`apply_rename` which execute a `cascade` plan.
- `crates/emuhub-tui/` — the ratatui app.
  - `main.rs` — terminal setup/teardown, the ~30fps input+redraw loop, key bindings. `handle_key` is a
    chain of early-returning overlay guards (ctrl-c → IP prompt → delete confirm → rename prompt →
    help → discovery → settings menu → saves browser → search) ahead of the normal-mode match; a new
    modal slots in as another guard. Ctrl-c is checked first and is the *only* quit — `q` closes
    overlays but does nothing in the browser, so a mistyped key can't end the session.
  - `app.rs` — `App` state + pure transition logic (`move_selection`, `toggle_favorite`, …), the part
    unit-tested (67 tests) since automating real raw-mode key input isn't practical here.
    Focus is three panes, `Consoles → Games → Detail`; the Details pane is not an overlay, so its
    action menu (`DetailState`) is a plain field rather than an `Option<…>` and `move_selection`
    routes to it when it holds focus. **Per-game actions (saves, favourite, rename, delete) have no
    key bindings** — the Details menu is their only entry point, deliberately, so there is one route
    to each rather than a menu plus hidden shortcuts to keep in sync.
  - `device.rs` — the device task: owns the one `Device` session, serializes SFTP ops, reports
    `DeviceEvent`s back over an `mpsc` channel. The UI task never awaits network I/O directly.
  - `search.rs` — `/` fuzzy search across every console at once, via `nucleo`.
  - `ui.rs` — pure `&App -> ratatui widgets` rendering. Lists render via `render_stateful_widget` with
    a `ListState` held in `App`, so long libraries scroll the selection into view. Floating modals all
    use `centered_rect` + `Clear`; the settings menu renders straight off `app::SettingsItem::ALL`, so
    adding a setting is one enum variant plus one dispatch arm in `main.rs` — no rendering change.
    The Details pane's action column works the same way off `DetailItem::ALL` /
    `GameSettingsItem::ALL`.
  - `src/bin/spike.rs` — standalone diagnostic against the real device, no TUI: connectivity, box
    art, play history, and a raw dump of the save tree for checking the Quirk 3 naming assumptions.

## Commands

```bash
cargo build --workspace          # debug build, ~5-10s incremental
cargo test --workspace           # 139 tests, no device needed
cargo clippy --workspace --all-targets
cargo run -p emuhub-tui --bin emuhub -- <miyoo-ip>     # run against real hardware
cargo run -p emuhub-tui --bin spike -- <miyoo-ip>      # device diagnostic (no TUI): connectivity,
                                                       # box art, recents, raw save-tree dump
```

### Installing `emuhub` on PATH

```bash
cargo install --path crates/emuhub-tui --bin emuhub --root ~/.local --debug
```

**Use `--debug`.** The workspace `[profile.release]` has `lto = true, codegen-units = 1` (fine for a
one-off optimized build), but `cargo install` without `--debug` runs that profile, and a full LTO
build of the crypto-heavy dependency tree (`russh`, `aws-lc-sys`, …) pegs every core for 7+ minutes. This is a network-bound TUI, not a compute-bound one — the debug build's runtime is
indistinguishable in practice and installs in ~6 seconds. Only drop `--debug` if you specifically want
to measure/ship an optimized binary.

`~/.local/bin` must be on `PATH`. Re-run the same install command after future code changes to update
it.

**If the AUR package is also installed, `/usr/bin/emuhub` may shadow this one** depending on `PATH`
order — check `which emuhub` before concluding a change didn't take effect.

## Device-protocol gotchas

Full writeup: `docs/DEVICE-PROTOCOL.md`, or the module docs above. The ones that will bite you if
forgotten:

- **NDJSON, not JSON.** `favourite.json`/`recentlist.json` — one object per line, no `[...]` wrapper.
- **Save states are filed by RetroArch core, not console folder.** `states/gpsp/`, not `states/GBA/`.
  Use `consoles::core_names_for()` — `saves.rs` already does.
- **A ROM is never just one file.** Deleting or renaming one without cascading to its box art, saves,
  favourite entry and recent entry leaves orphans — the bug the Swift original shipped. Always go
  through `cascade::delete_plan`/`rename_plan`; never remove a ROM path directly.
- **Onion caches the ROM list per console** in `Roms/{CONSOLE}/{CONSOLE}_cache6.db`. Left alone after
  a delete/rename it keeps showing the old entry on the handheld. The cascade renames it to `.bak` so
  Onion rebuilds it — renamed, not deleted, so a bad rebuild is recoverable.

## russh / dropbear connection gotchas

The Miyoo's dropbear is old and won't negotiate with `russh`'s modern defaults. Required in
`transport.rs`'s `Preferred`:

- Cipher `aes128-ctr`, kex `diffie-hellman-group14-sha1`/`-sha256` — from the Swift original.
- **MAC `hmac-sha1`** — not in russh's default preferred list at all; found by hitting `No common Mac
  algorithm` against the real device. Without this, connection fails during algorithm negotiation, not
  auth — easy to misdiagnose as a credentials problem.
- Auth: `root` with an empty password; some dropbear builds want the `none` method instead — `connect()`
  tries both.

## App-level gotchas (bugs already hit once — don't reintroduce)

- **Image identity vs. location.** `DeviceRequest::FetchImage`/`DeviceEvent::ImageBytes` carry a `key`
  (what the UI files the image under — a ROM path for box art, a thumbnail path for a save state), not
  the PNG's own path. The single-slot `app.image_state`/`image_for_key` is keyed on that throughout, and
  `App::current_image` is the one place deciding which is wanted. Conflating key and location means the
  detail pane's "is this the selected game's art?" check can never be true — a real shipped bug (box art
  decoded successfully but never rendered).
- **No "already requested" gate on image fetch.** `image_state` holds only the *last* decoded image;
  navigating away evicts it. A permanent "requested this session" flag (removed — see git history if it
  reappears) makes a previously-viewed game's art un-refetchable forever after you look at a third game.
  Re-requesting on every selection change is intentional and cheap: the disk cache in `cache.rs` serves
  repeat fetches back near-instantly. Same reasoning covers the saves browser evicting box art and back.
- **Virtual consoles are not the library.** Recently Played (index 0) and Favourites (index 1) are
  `ConsoleEntry`s with `kind: ConsoleKind::Virtual`, holding the same `GameFile`s as the real consoles.
  Skip them in `search.rs` (or every recent/favourited game scores twice) and in `persist_cache` (or the
  next launch reads back a `__RECENT` folder that doesn't exist). Never hardcode a console index — look
  it up by folder, as the tests do. **There is more than one virtual row**, so anything targeting a
  specific one must match on `folder`, never on `kind == Virtual`: that returns whichever comes first
  and quietly paints the favourites into Recently Played (`App::set_virtual_console` is the one place
  that does this lookup).
- **A favourite doesn't have to resolve to a scanned ROM.** Both virtual rows rebuild an unmatched
  entry via `scan::parse_rom_path` rather than dropping it, so the app shows what the *device* believes.
  A real card in use has a `favourite.json` entry whose filename differs from the ROM's by a double
  space — it appears in Favourites and is never starred in the library, which is correct: the device
  has a stale favourite.
- **The LAN is not a /24, and the handheld is slow to wake.** Discovery originally swept
  `a.b.c.1-254` around our own address with a 400ms probe timeout. Plenty of home networks are wider
  than a /24 (eero hands out `192.168.68.0/22` by default), so three quarters of such a network was
  never probed, and the Miyoo's wifi
  power-save routinely takes over 400ms to answer the first SYN, so even a correctly-addressed probe
  missed it intermittently. `discover.rs` now takes the prefix length from the interface (`if-addrs`)
  and allows 1.5s per probe. When a sweep comes up empty, the modal names the range it swept — check
  that line first.
- **`tracing::warn!` is invisible while the TUI is up.** The subscriber writes to stderr underneath
  the alternate screen with `EnvFilter` off unless `RUST_LOG` is set. The save-state listing failed
  that way for a whole release: every game reported "no save states" and the reason never reached the
  screen. Device-task failures belong in `DeviceEvent::Error`, not the log.
- **Anything loaded only on connect must also be cached and re-requestable.** The save listing ran
  once inside `do_connect` and wasn't in `AppCache`, so a session that started with the handheld
  asleep showed a full cached library in which nothing had saves, with no way to retry short of a
  restart. It's now cached like `recents` and refreshable via `DeviceRequest::LoadSaveStates`.
- **`Picker::from_query_stdio()` must run after `enable_raw_mode()`/`EnterAlternateScreen`.** It queries
  the terminal for Kitty/Sixel support and needs to read the escape-sequence response synchronously;
  in cooked mode the query silently fails and falls back to halfblocks. `main.rs` enters raw mode
  *before* constructing `App` (which builds the `Picker`) for exactly this reason — keep it that way,
  and keep every fallible non-terminal setup step (config/cache loading) *before* that point, so an
  early `?` never leaves the terminal stuck in raw mode.

## Testing

`cargo test --workspace` — no device required; `emuhub-core`'s tests run entirely against fixtures in
`tests/fixtures/`, `emuhub-tui`'s `app.rs` tests drive `App` state transitions directly (see note above
on why — raw-mode key input can't be automated here, so these substitute for interactive
keypress testing; a real interactive check in an actual terminal is still worth doing after UI changes).

## Deployment

None — this is a local CLI tool (`emuhub`), not a deployed service.
