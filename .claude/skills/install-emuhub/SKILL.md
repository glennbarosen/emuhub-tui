---
name: install-emuhub
description: Build and install (or update) the emuhub binary onto PATH. Use when the user says "install it", "update the installed version", "rebuild emuhub", "put it on PATH", or after a code change they'll want to try via the `emuhub` command rather than `cargo run`.
---

# Install emuhub Skill

```bash
cargo install --path crates/emuhub-tui --bin emuhub --root ~/.local --debug
```

**Always use `--debug` unless the user specifically asks for an optimized build.** The workspace's
`[profile.release]` sets `lto = true, codegen-units = 1`, so `cargo install` *without* `--debug`
compiles the crypto-heavy dependency tree (`russh`, `aws-lc-sys`, …) with whole-program LTO — 7+
minutes pegging every core, once measured directly. `emuhub` is a network-bound TUI
waiting on SSH/SFTP round trips, not a compute-bound one; the debug build's runtime is indistinguishable
in practice and installs in about 6 seconds. If the user wants the optimized build anyway (e.g. to
actually measure or ship it), warn them it'll take a while and consume real CPU before running it
without `--debug`.

`~/.local/bin` must be on `PATH`. Re-running the same command after future code changes overwrites the
installed binary in place.

## Verify

```bash
which emuhub
```

**If this prints `/usr/bin/emuhub`, the AUR package is shadowing the dev build** — `/usr/bin` comes
before `~/.local/bin` on many `PATH`s, so the user would be running the released binary while believing
they were testing local changes. Say so and offer to `rm ~/.local/bin/emuhub` or uninstall the package.

Don't try to run `emuhub --help` or similar as a smoke test from a non-interactive shell — it needs a
real TTY (raw mode) and will fail with an unrelated-looking `No such device or address` error that has
nothing to do with whether the install succeeded. Confirming the binary exists and is executable (or
`ls -la ~/.local/bin/emuhub`) is the right level of verification here; actually running it belongs to
[[run-against-device]] in a real terminal.
