# Miyoo Mini+ / Onion OS device protocol

Where Onion OS keeps things on the SD card, and the several non-obvious file-format quirks that make
writing to it harder than it looks. This was reverse-engineered from the SwiftUI
[`glennbarosen/emuhub`](https://github.com/glennbarosen/emuhub) and its `MIYOO_STRUCTURE.md`, then
verified against a real device.

It's written down here because it isn't documented anywhere else, and because getting any of §3–§5
wrong produces *silent* breakage — a favourites list the handheld quietly ignores, or save states the
app swears don't exist. If you're porting this to another language or writing your own tool, this file
is the part worth taking.

Everything here is implemented in `crates/emuhub-core/`; where a section names the module, that's the
authoritative version.

---

## 1. Connection

- **Host**: the device's LAN IP, shown on-device under `Apps › Tweaks › Network`. SSH must be enabled
  there too.
- **Auth**: user `root`, **empty password**. That is the Onion OS default and there's no way to set a
  password through the stock UI — treat the handheld as a device you only ever expose to your own LAN.
- **The device runs an old dropbear** that won't negotiate with a modern SSH client's defaults. You
  must explicitly allow:

  | | |
  |---|---|
  | Cipher | `aes128-ctr` |
  | Key exchange | `diffie-hellman-group14-sha1`, `diffie-hellman-group14-sha256` |
  | MAC | `hmac-sha1` |
  | Host key | accept anything (the device regenerates its key on some updates) |

  The MAC line is the one that costs people an afternoon. It isn't in `russh`'s default preferred list
  at all, and without it negotiation fails with `No common Mac algorithm` — *during algorithm
  negotiation, not authentication*, which reads exactly like a wrong-credentials error and sends you
  debugging the wrong thing.

- Some dropbear builds want the `none` auth method rather than an empty password. Try both.

To reach the device with the stock `ssh` client instead, you need the equivalent in `~/.ssh/config`:

```
Host miyoo
    HostName 192.168.1.50
    User root
    HostKeyAlgorithms +ssh-rsa
    KexAlgorithms +diffie-hellman-group14-sha1
    MACs +hmac-sha1
    Ciphers +aes128-ctr
```

Implementation: `crates/emuhub-core/src/transport.rs`.

---

## 2. Key paths

| Purpose | Path |
|---|---|
| SD card root | `/mnt/SDCARD` |
| ROMs | `/mnt/SDCARD/Roms/{CONSOLE}/` |
| Box art | `/mnt/SDCARD/Roms/{CONSOLE}/Imgs/{rom-basename}.png` |
| Favourites | `/mnt/SDCARD/Roms/favourite.json` |
| Play history | `/mnt/SDCARD/Roms/recentlist.json` **or** `recentlist-hidden.json` |
| Per-console ROM cache | `/mnt/SDCARD/Roms/{CONSOLE}/{CONSOLE}_cache6.db` |
| Save files (`.sav`/`.srm`) | `/mnt/SDCARD/Saves/CurrentProfile/saves/{CORE}/` |
| Save states | `/mnt/SDCARD/Saves/CurrentProfile/states/{CORE}/` |
| State thumbnails | same directory, `{statefile}.png` (e.g. `Game.state21.png`) |
| Launch script | `/mnt/SDCARD/Emu/{CONSOLE}/launch.sh` |
| Console icons | `/mnt/SDCARD/Icons/Default/` |
| BIOS | `/mnt/SDCARD/BIOS/` |

Note `{CORE}` in the saves paths — see §5, it is not `{CONSOLE}`.

---

## 3. Quirk 1 — `favourite.json` is NDJSON, not JSON

One JSON object **per line**. No array wrapper, no commas between entries:

```json
{"label":"Pokemon - Emerald","launch":"/mnt/SDCARD/Emu/GBA/launch.sh","type":5,"imgpath":"/mnt/SDCARD/Roms/GBA/Imgs/Pokemon - Emerald.png","rompath":"/mnt/SDCARD/Roms/GBA/Pokemon - Emerald.gba"}
{"label":"Wario Land 4","launch":"/mnt/SDCARD/Emu/GBA/launch.sh","type":5,"imgpath":"/mnt/SDCARD/Roms/GBA/Imgs/Wario Land 4.png","rompath":"/mnt/SDCARD/Roms/GBA/Wario Land 4.gba"}
```

**Writing a well-formed JSON array silently breaks the favourites list on the device.** No error, no
crash — the handheld just shows an empty Favourites section. Round-trip against a captured fixture
rather than trusting your serializer.

Fields:

| Field | Meaning |
|---|---|
| `label` | ROM filename minus its extension |
| `launch` | `/mnt/SDCARD/Emu/{CONSOLE}/launch.sh` |
| `type` | `5` for favourites, `0` for recents |
| `imgpath` | `{rom-dir}/Imgs/{label}.png` — optional |
| `rompath` | absolute path to the ROM, but see §4 |

`recentlist.json` uses the identical format. Entries **without** a `rompath` are app launches rather
than games and must be filtered out.

Implementation: `crates/emuhub-core/src/favorites.rs`.

---

## 4. Quirk 2 — ROM paths contain `../../` segments

The device writes relative segments into `rompath`, so the same ROM can legitimately appear as either
of these:

```
/mnt/SDCARD/Roms/GBA/Wario Land 4.gba
/mnt/SDCARD/Roms/GBA/../../Roms/GBA/Wario Land 4.gba
```

They must be normalized before comparing against a path from a directory listing, or favourites will
never match the games they point at. The rule is a plain lexical resolve — do not touch the filesystem,
these paths are remote:

```
split on "/", drop empty segments
  ".."  -> pop the last segment
  "."   -> skip
  else  -> push
rejoin with a leading "/"
```

Implementation: `crates/emuhub-core/src/path.rs`.

---

## 5. Quirk 3 — save states are filed by RetroArch core, not console folder

GBA save states do not live in `states/GBA/`. They live in `states/gpsp/` or `states/mgba/`, depending
on which core actually ran the game. Resolution order: check the console-folder-named directory first,
then each known core in priority order, then fall back to scanning every subdirectory.

| Console | Cores, in priority order |
|---|---|
| `GB` | `gambatte`, `gearboy`, `tgbdual` |
| `GBC` | `gambatte`, `gearboy`, `tgbdual` |
| `GBA` | `gpsp`, `mgba`, `vba_next` |
| `FC` | `fceumm`, `nestopia` |
| `SFC` | `snes9x2005_plus`, `snes9x2005`, `snes9x2010`, `mednafen_supafaust` |
| `MD` | `picodrive`, `genesis_plus_gx` |
| `MS` | `picodrive`, `smsplus`, `genesis_plus_gx` |
| `PS` | `pcsx_rearmed` |
| `NDS` | `drastic` |
| `PCE` | `mednafen_pce_fast` |
| `PCECD` | `mednafen_pce_fast` |
| `GG` | `genesis_plus_gx`, `smsplus` |
| `NEOGEO` | `fbalpha2012_neogeo`, `fbneo` |
| `NGP` | `mednafen_ngp`, `race` |
| `ARCADE` | `mame2003_plus`, `fbalpha2012` |
| `ATARI` | `stella2014` |
| `LYNX` | `handy` |
| `WS` | `mednafen_wswan` |
| `VB` | `mednafen_vb` |
| `MSX` | `bluemsx`, `fmsx` |
| `COMMODORE` | `vice_x64` |
| `AMIGA` | `puae`, `uae4arm` |
| `DOS` | `dosbox_pure` |
| `PICO` | `fake08`, `retro8` |

**State filenames**: `{Game}.state`, `{Game}.state{N}`, `{Game}.state.auto`. The slot number is the
digits after `.state`; both `.auto` and a bare `.state` mean slot 0. Each state may have a
`{statefile}.png` sitting beside it — that's its thumbnail.

`.sav` and `.srm` are save *files*, not states, and live under `saves/` rather than `states/`. When
scanning either directory, exclude these extensions or you'll pick up thumbnails and RetroArch config
litter as if they were saves:

```
png jpg jpeg bmp bak cfg opt log txt xml json
```

Implementation: `crates/emuhub-core/src/saves.rs`; the table itself is
`CONSOLE_CORE_NAMES` in `crates/emuhub-core/src/consoles.rs`.

---

## 6. Quirk 4 — a ROM is never just one file

Deleting or renaming a ROM by touching only its own path leaves orphans scattered across the card. The
full set that has to move with it:

1. The ROM file itself.
2. Its box art, `{console}/Imgs/{basename}.png`.
3. Every save state and save file across all candidate core directories (§5) — plus each state's `.png`
   thumbnail.
4. Its entry in `favourite.json`, if present.
5. Its entry in `recentlist.json`, if present.
6. **`Roms/{CONSOLE}/{CONSOLE}_cache6.db`** — Onion caches the ROM list per console. Left alone, the
   handheld keeps showing the deleted game until something forces a rebuild. Rename it to `.bak` rather
   than deleting it, so a bad rebuild is recoverable.

This is the bug the SwiftUI original shipped, and it's why `emuhub-core` computes the whole cascade as a
pure plan before executing any of it.

Implementation: `crates/emuhub-core/src/cascade.rs`.

---

## 7. Console list

Folder name → display name. The folder is the literal `Roms/{FOLDER}/` directory component on the
device, not a slug you get to choose.

| Folder | Console | | Folder | Console |
|---|---|---|---|---|
| `GB` | Game Boy | | `GG` | Game Gear |
| `GBC` | Game Boy Color | | `WS` | WonderSwan |
| `GBA` | Game Boy Advance | | `VB` | Virtual Boy |
| `FC` | NES / Famicom | | `MSX` | MSX |
| `SFC` | Super Nintendo | | `COMMODORE` | Commodore 64 |
| `MD` | Sega Genesis | | `AMIGA` | Amiga |
| `MS` | Sega Master System | | `DOS` | DOS |
| `PS` | PlayStation | | `SCUMMVM` | ScummVM |
| `ARCADE` | Arcade (MAME) | | `PICO` | PICO-8 |
| `ATARI` | Atari 2600 | | `NDS` | Nintendo DS |
| `LYNX` | Atari Lynx | | `PCE` | TurboGrafx-16 |
| `NEOGEO` | Neo Geo | | `PCECD` | TurboGrafx CD |
| `NGP` | Neo Geo Pocket | | | |

ROM extensions the scanner accepts:

```
gb gbc gba nes snes sfc md gen sms gg nds psx bin cue iso zip 7z chd
```

A console registered without its extension here shows a permanent "0 roms" with no error anywhere —
it looks exactly like an empty SD card folder rather than a bug.

Implementation: `crates/emuhub-core/src/consoles.rs`. See `.claude/skills/add-console/SKILL.md` for
the three places a new console has to be registered.

---

## 8. Scanning efficiently

The whole library is one `find` over `/mnt/SDCARD/Roms`, parsed locally — not a directory listing per
console. The SwiftUI original walked all 25 consoles with a separate SFTP round trip each, which is
most of why it felt slow over wifi.

`emuhub` asks for `stat -c '%s %n'` output to get file sizes in the same pass, and falls back to a bare
`find` when the device's busybox doesn't support it. Save states get the same treatment: one `find`
over `states/` and `saves/` together.

Implementation: `crates/emuhub-core/src/scan.rs`, `crates/emuhub-core/src/saves.rs`.
