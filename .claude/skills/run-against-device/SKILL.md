---
name: run-against-device
description: Test emuhub-tui or a specific change against the real Miyoo Mini+ handheld over SSH/SFTP. Use when the user wants to "test this on the device", "check if it works on hardware", "verify the fix", or when debugging why the app can't connect, sync favourites, or show box art. Also covers distinguishing a real code bug from the device just being asleep/offline.
---

# Run Against Device Skill

Verify a change against the real Miyoo Mini+, without over-building test infrastructure for what
should be a quick check.

## Step 1 — confirm the device is actually reachable

Onion OS handhelds sleep and drop Wi-Fi routinely; this is expected, not a bug. Rule it out first so
you don't chase a phantom code regression:

```bash
timeout 5 bash -c "echo > /dev/tcp/<ip>/22" && echo reachable || echo "not reachable"
```

If unreachable: say so plainly and stop — don't touch code. The app's own offline mode (cache-first
browsing) is *supposed* to degrade gracefully here; that's a feature, not something to route around.

## Step 2 — prefer the non-interactive `spike` binary for anything automatable

```bash
cargo run -p emuhub-tui --bin spike -- <ip>
```

It already: connects with the legacy SSH algorithms, lists the whole library (one `find` round trip),
reads favourites, checks for `Imgs/` box-art directories, and fetches one image end-to-end — printing
plain stdout you can read directly. For a new one-off check (e.g. "does this specific SFTP path exist"),
extend `spike.rs` with another `device.exec("...")` or `device.fetch_image(...)` call rather than adding
a throwaway script elsewhere — `Device::exec` is `pub` specifically so ad-hoc device debugging doesn't
need a new transport method each time.

## Step 3 — for the full interactive TUI, ask the human

**Do not build a pty/tmux harness to simulate keypresses.** This was tried once: raw-mode key input
never reached crossterm at all (confirmed via `tracing` — zero events, even though output, connect, and
reconnect all worked fine over the same pty), so it only burned effort chasing a harness artifact
instead of the real bug. It also missed a real bug that direct `App` unit tests then
caught in seconds (see `crates/emuhub-tui/src/app.rs`'s `#[cfg(test)] mod tests`).

Instead:

1. Do everything automatable: `cargo build --workspace`, `cargo test --workspace`, `cargo clippy
   --workspace --all-targets`, and a `spike` run against real hardware for transport/data-layer changes.
2. For UI/interaction logic changes, add or extend an `App` state-transition test in `app.rs` — it
   exercises exactly what a keypress does, without a terminal.
3. Hand the user one short concrete command and ask them to check the specific thing that changed —
   e.g. "run `emuhub <ip>`, navigate to X, confirm Y." Don't ask for a full click-through; name the
   exact thing to look at.

## Reading device-side failures

- `No common Mac algorithm` / kex / cipher errors during `Device::connect` → algorithm negotiation
  problem, not a credentials problem (see `AGENTS.md`'s russh/dropbear gotchas). Don't touch auth code.
- `fetch_image` returning `None` → could be a genuinely missing file (not every ROM has scraped box
  art) or a real regression. Use `spike`'s box-art check (it lists `Imgs/` contents and file counts) to
  tell the two apart before assuming a bug.
