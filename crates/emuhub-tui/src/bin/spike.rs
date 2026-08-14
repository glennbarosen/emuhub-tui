//! Transport spike: the riskiest unknown in this
//! rewrite is whether `russh` can actually negotiate with the Miyoo's old
//! dropbear using the legacy algorithms the Swift original had to force.
//! This is deliberately the only thing this binary does — go/no-go on the
//! whole `russh` transport decision before any TUI code depends on it.
//!
//! Usage: `cargo run -p emuhub-tui --bin spike -- <host> [port] [username]`
//! Defaults: port 22, username root.
//!
//! Fallback if this fails: shell out to `ssh`/`sftp` with
//! `HostKeyAlgorithms +ssh-rsa`, `KexAlgorithms +diffie-hellman-group14-sha1`,
//! `Ciphers +aes128-ctr` in `~/.ssh/config` — see `docs/DEVICE-PROTOCOL.md` §1.

use emuhub_core::transport::Device;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = std::env::args().skip(1);
    let host = args.next().ok_or_else(|| anyhow::anyhow!("usage: spike <host> [port] [username]"))?;
    let port: u16 = args.next().map(|p| p.parse()).transpose()?.unwrap_or(22);
    let username = args.next().unwrap_or_else(|| "root".to_string());

    println!("Connecting to {username}@{host}:{port} with legacy algorithms (aes128-ctr / dh-group14-sha1+sha256)...");
    let device = Device::connect(&host, port, &username).await?;
    println!("✓ SSH connected, SFTP subsystem opened");

    println!("Listing /mnt/SDCARD/Roms (single find exec)...");
    let games = device.list_all_roms().await?;
    println!("✓ Found {} ROM(s) across the library", games.len());
    for game in games.iter().take(10) {
        let size = game.size.map(|b| format!("{b} B")).unwrap_or_else(|| "size unknown".to_string());
        println!("  [{}] {} ({size})", game.console_folder, game.name);
    }
    if games.len() > 10 {
        println!("  ... and {} more", games.len() - 10);
    }

    // `list_all_roms` asks `stat` for sizes and silently falls back to a plain
    // `find` if this busybox can't do it — which looks identical from the
    // outside except that every size is `None`. Check the command directly so
    // the fallback can't hide.
    println!("\nSize support — does this busybox have `stat -c`?");
    let sized = games.iter().filter(|g| g.size.is_some()).count();
    println!("  {sized} of {} ROM(s) came back with a size", games.len());
    let probe = device
        .exec("find /mnt/SDCARD/Roms -maxdepth 2 -type f -exec stat -c '%s %n' {} + 2>/dev/null | head -3")
        .await?;
    if probe.trim().is_empty() {
        println!("  ✗ `stat -c` produced nothing — the plain-find fallback is in use");
    } else {
        println!("  ✓ raw output:");
        for line in probe.lines().take(3) {
            println!("    {line}");
        }
    }

    println!("Reading favourite.json...");
    let favorites = device.read_favorites().await?;
    println!("✓ Parsed {} favourite(s)", favorites.len());
    for fav in favorites.iter().take(5) {
        println!("  ★ {} -> {}", fav.label, fav.normalized_path());
    }

    println!("\nBox art check — do Imgs/ folders exist and have files?");
    let imgs_dirs = device.exec("find /mnt/SDCARD/Roms -maxdepth 2 -type d -iname Imgs").await?;
    println!("Imgs/ directories found:\n{imgs_dirs}");
    let imgs_files = device.exec("find /mnt/SDCARD/Roms -maxdepth 3 -path '*/Imgs/*' -type f").await?;
    let file_count = imgs_files.lines().filter(|l| !l.trim().is_empty()).count();
    println!("Box art files found: {file_count}");
    for line in imgs_files.lines().take(5) {
        println!("  {line}");
    }
    if let Some(first_game) = games.first() {
        println!("\nTrying to fetch box art for: {}", first_game.image_path);
        match device.fetch_image(&first_game.image_path).await {
            Some(data) => println!("✓ Fetched {} bytes", data.len()),
            None => println!("✗ fetch_image returned None (file missing or SFTP read failed)"),
        }
    }

    println!("\nPlay history (recentlist.json / recentlist-hidden.json)...");
    let recents = device.read_recents().await?;
    println!("✓ Parsed {} recent entr(ies)", recents.len());
    for entry in recents.iter().take(5) {
        println!("  🕘 {} -> {}", entry.label, entry.normalized_path().unwrap_or_default());
    }

    // Quirk 3 recon. The parser here is built on the documented
    // naming (`Game.state21`, thumbnail `Game.state21.png`, cores not console
    // folders) — this dumps the raw listing so those assumptions can be
    // checked against a real card rather than trusted.
    println!("\nSave tree — raw listing (verifies the Quirk 3 core-dir naming):");
    let raw = device
        .exec("find /mnt/SDCARD/Saves/CurrentProfile/states /mnt/SDCARD/Saves/CurrentProfile/saves -type f 2>/dev/null")
        .await?;
    let raw_count = raw.lines().filter(|l| !l.trim().is_empty()).count();
    println!("  {raw_count} file(s) under states/ + saves/");
    for line in raw.lines().take(20) {
        println!("    {line}");
    }

    let states = device.list_save_states().await?;
    println!("✓ Parsed {} save state(s)/file(s)", states.len());
    let with_thumbs = states.iter().filter(|s| s.thumbnail_path.is_some()).count();
    println!("  {with_thumbs} have a screenshot");
    for state in states.iter().take(10) {
        println!("    {} · {} ({})", state.game_name, state.display_name(), state.name);
    }

    // Cross-check: how many of the parsed saves actually bind to a scanned
    // ROM? A low number here means the basename matching needs revisiting.
    let (matched, unmatched): (Vec<_>, Vec<_>) =
        games.iter().partition(|g| !emuhub_core::saves::states_for_game(&states, g).is_empty());
    println!("  {} of {} scanned ROM(s) matched at least one save", matched.len(), games.len());
    // Naming the misses is the difference between "the matching is fine" and a
    // whole console whose emulator files saves somewhere else entirely (NDS
    // under DraStic being the suspect).
    for game in &unmatched {
        println!("  ✗ no saves matched: [{}] {}", game.console_folder, game.name);
    }

    if let Some(thumb) = states.iter().find_map(|s| s.thumbnail_path.clone()) {
        println!("\nFetching a save-state screenshot: {thumb}");
        match device.fetch_image(&thumb).await {
            Some(data) => println!("✓ Fetched {} bytes", data.len()),
            None => println!("✗ fetch_image returned None"),
        }
    }

    println!("\nGO: transport spike succeeded.");
    Ok(())
}
