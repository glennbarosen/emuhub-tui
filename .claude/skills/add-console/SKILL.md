---
name: add-console
description: Add a new emulator console/system to emuhub-tui, or add save-state core support to an existing one. Use when the user says "add support for X console", "add [system name]", "add a new emulator/platform", or wants save states working for a console that isn't finding them. Covers the three places a console is registered and the failure mode of missing any one of them.
---

# Add Console Skill

All device-protocol knowledge for consoles lives in one place:
[`crates/emuhub-core/src/consoles.rs`](../../../crates/emuhub-core/src/consoles.rs). Three tables,
each independent — a console can exist in one without the others.

## 1. Register the console — `ALL_SYSTEMS`

```rust
Console { name: "Display Name", folder: "FOLDER", icon: "🎮" },
```

- **`folder` must exactly match the on-device `Roms/{FOLDER}/` directory name** used by Onion OS —
  this isn't a free-form slug, it's the literal remote path component. Check the console table in
  `docs/DEVICE-PROTOCOL.md` §7; if genuinely unknown, ask the user rather than guessing (a wrong folder name means the console silently shows 0 ROMs forever — SFTP just
  returns nothing for a directory that doesn't exist, no error surfaces).
- **List order is sidebar order**, not alphabetical — it was preserved verbatim from the Swift
  original's `Console.allSystems`. Don't resort it; append new entries, or ask before reordering.
- `icon` is a single emoji, matching the existing entries' style.

## 2. Register any new ROM file extensions — `ROM_EXTENSIONS`

Only needed if the console uses an extension not already listed (most don't — many consoles share
`.zip`/`.bin`/`.iso`). **This is the failure mode that bites hardest**: `scan.rs`'s library scanner
silently drops any file whose extension isn't in this list. A console registered in `ALL_SYSTEMS` but
missing its extension here will show up in the sidebar with a permanent "0 roms" and no error anywhere
— it looks exactly like an empty SD card folder, not a bug. Add the extension, lowercase, no dot.

## 3. Register save-state cores — `CONSOLE_CORE_NAMES` (optional)

Save states are filed by **RetroArch core name**, not console folder
(`states/gpsp/`, not `states/GBA/`) — this table maps folder → candidate core names, checked in
priority order. If you don't know the core name(s) Onion OS ships for this console, it's fine to leave
the console out of this table entirely; nothing else depends on it, and a future fallback path can scan
all subdirectories. Don't guess a core name — a wrong one just means save states silently aren't found,
same failure shape as a missing extension.

## Verify

```bash
cargo test -p emuhub-core
```

Existing structural tests catch the common mistakes automatically:

- `every_console_has_a_unique_folder` — duplicate/typo'd folder name
- `every_console_with_cores_is_a_known_console` — a `CONSOLE_CORE_NAMES` entry for a console that
  isn't in `ALL_SYSTEMS`
- `rom_extension_check_is_case_insensitive` — sanity on the extension matcher itself

If you added a new extension, add a one-line case to that test file mirroring the existing ones. If
you have a real `find` output sample from the device for the new console, extending the
`tests/fixtures/find_roms.txt` fixture and `scan::tests::parses_captured_find_fixture`'s expected count
is the most direct way to prove the whole scan path works end to end for it.
