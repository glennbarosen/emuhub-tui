//! Finding the handheld on the LAN without being told its IP.
//!
//! The Swift original used Bonjour (`NWBrowser`), which has no Linux
//! equivalent worth the dependency, so this takes the other approach the
//! original also supported — concurrent TCP probes of port 22 across the
//! local network.
//!
//! An open port 22 is not proof of anything, though: a NAS, a router, or a
//! development box will all answer. So the sweep is only stage one. Stage two
//! actually authenticates and looks for `/mnt/SDCARD`, which turns "something
//! is listening" into "this is the handheld" — worth the extra second, since
//! the whole point is to hand the user an address they can trust.
//!
//! Two things this got wrong at first, both of which look identical from the
//! outside ("device is on, scan finds nothing"):
//!
//! * **The local network is not always a /24.** This one assumed it was, and
//!   swept `a.b.c.1-254` around our own address. Plenty of consumer routers
//!   (eero's default is `192.168.68.0/22`) hand out leases across a wider
//!   range, so a handheld one octet over was invisible. The prefix length now
//!   comes from the interface itself.
//! * **A sleepy wifi client is slow to answer the first packet.** The probe
//!   timeout was 400ms, which the handheld misses whenever its radio is in
//!   power-save — the same device that a plain `ssh` reaches fine a second
//!   later. See `PROBE_TIMEOUT`.
//!
//! And when stage two *does* reject a host, it says why now
//! (`DiscoveryUpdate::Rejected`) rather than silently dropping it — an SSH
//! host that fails auth is a very different problem from an empty network,
//! and the modal should not present them the same way.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::task::JoinSet;

use crate::transport::Device;

/// How long a single host gets to complete a TCP handshake.
///
/// Was 400ms on the theory that a LAN device answers in single-digit
/// milliseconds. It does — when it's awake. The Miyoo's wifi power-save means
/// the first SYN after an idle period routinely takes over a second to come
/// back, and 400ms turned "found" into "not found" run to run against a
/// device that `spike` connected to seconds earlier. Nonexistent hosts never
/// answer at all, so this timeout is also the sweep's floor: keep
/// `CONCURRENCY` high enough that the whole range still fits in a few waves.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Stage two's budget. An SSH handshake plus one `exec` against the handheld
/// is ~1s; anything past this is a host that opened the port and then stopped
/// talking, and it must not hold a probe slot open forever — without this the
/// sweep itself can hang rather than finish empty.
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// Probes in flight at once. High enough to sweep even a /22 in a handful of
/// waves, low enough not to exhaust file descriptors or make a cheap router
/// drop traffic.
const CONCURRENCY: usize = 256;

/// Ceiling on how many addresses a sweep will probe. A /22 (1022 hosts) is
/// about the largest a home router hands out and takes ~6s; past that the
/// range is almost certainly a `/16`-style corporate or container network
/// where a brute sweep is the wrong tool, so it's narrowed to the widest
/// range under this ceiling around our own address — a /22 — rather than
/// left to run for minutes.
const MAX_HOSTS: usize = 1024;

/// What a sweep reports back as it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryUpdate {
    /// One more host finished probing (whether it answered or not).
    Progress {
        done: usize,
        total: usize,
    },
    /// A host answered on the SSH port *and* proved it has an SD card mounted.
    Found {
        host: String,
    },
    /// A host answered on the SSH port but failed the identity check. Carried
    /// through to the UI because "something is there but it isn't the
    /// handheld (or wouldn't let us in)" is actionable, and silently dropping
    /// it makes a credentials problem look like an empty network.
    Rejected {
        host: String,
        reason: String,
    },
    Done,
}

/// The IPv4 network this machine is on: the address the kernel would use to
/// reach the outside world, plus the prefix length of the interface that owns
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNetwork {
    /// Our own address on that interface.
    pub local: Ipv4Addr,
    /// The interface's prefix length — /24, /22, whatever it actually is.
    pub prefix_len: u8,
}

impl LocalNetwork {
    /// The network address (host bits cleared).
    pub fn network(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.local) & self.mask())
    }

    /// The broadcast address (host bits set).
    pub fn broadcast(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.local) | !self.mask())
    }

    /// Built from the *effective* prefix, so `network`, `broadcast`, `label`
    /// and `hosts` all describe the same range — the one actually swept.
    /// Deriving them from the raw interface prefix instead left a clamped
    /// sweep filtering against a network/broadcast pair that wasn't even
    /// inside the range it was probing.
    fn mask(&self) -> u32 {
        let len = self.effective_prefix_len();
        // A /0 would overflow the shift; clamping keeps that impossible, but
        // the guard stays so a weird interface can't panic the sweep.
        if len == 0 {
            0
        } else {
            u32::MAX << (32 - len.min(32))
        }
    }

    /// `192.168.68.0/22`-style label, for telling the user what is actually
    /// being swept. When the scan misses a device that is plainly switched
    /// on, this line is the first thing worth checking.
    pub fn label(&self) -> String {
        format!("{}/{}", self.network(), self.effective_prefix_len())
    }

    /// The prefix actually swept, after `MAX_HOSTS` clamping.
    fn effective_prefix_len(&self) -> u8 {
        let mut len = self.prefix_len.min(32);
        while len < 32 && host_count(len) > MAX_HOSTS {
            len += 1;
        }
        len
    }

    /// Every host address to probe: the whole network minus its network and
    /// broadcast addresses, and minus ourselves — we are not the handheld,
    /// and probing our own SSH port only produces a confusing extra hit.
    pub fn hosts(&self) -> Vec<String> {
        let len = self.effective_prefix_len();
        let base = u32::from(self.network());
        let size = if len >= 32 { 1u64 } else { 1u64 << (32 - len) };

        (0..size)
            .map(|offset| Ipv4Addr::from(base.wrapping_add(offset as u32)))
            // On a /31 or /32 there is no network/broadcast pair to skip;
            // anywhere else the first and last address are not hosts.
            .filter(|addr| size < 4 || (*addr != self.network() && *addr != self.broadcast()))
            .filter(|addr| *addr != self.local)
            .map(|addr| addr.to_string())
            .collect()
    }
}

fn host_count(prefix_len: u8) -> usize {
    if prefix_len >= 31 {
        return 2;
    }
    (1usize << (32 - prefix_len)).saturating_sub(2)
}

/// The local network, derived from whichever interface would be used to reach
/// the internet.
///
/// Finding *which* address is ours uses the connected-UDP-socket trick rather
/// than picking the first non-loopback interface: `connect` on a UDP socket
/// only sets the peer address and picks a route, so no packet is ever sent,
/// and it answers "which interface would actually be used" — the thing that
/// matters on a machine with docker bridges, VPNs and a wifi card all up at
/// once. The prefix length then comes from `getifaddrs` for the interface
/// holding that address; guessing /24 there is what hid the handheld on a /22
/// network.
pub fn local_network() -> Option<LocalNetwork> {
    let local = local_ipv4()?;
    Some(LocalNetwork { local, prefix_len: prefix_len_for(local).unwrap_or(24) })
}

fn local_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(addr) => Some(addr),
        IpAddr::V6(_) => None,
    }
}

fn prefix_len_for(local: Ipv4Addr) -> Option<u8> {
    let interfaces = if_addrs::get_if_addrs()
        .inspect_err(|err| tracing::warn!(%err, "couldn't enumerate interfaces; assuming /24"))
        .ok()?;

    interfaces.into_iter().find_map(|iface| match iface.addr {
        if_addrs::IfAddr::V4(v4) if v4.ip == local => Some(v4.prefixlen),
        _ => None,
    })
}

/// The `a.b.c` prefix of an IPv4 address.
pub fn subnet_prefix_of(addr: Ipv4Addr) -> String {
    let [a, b, c, _] = addr.octets();
    format!("{a}.{b}.{c}")
}

/// Sweeps `network` for the handheld, reporting progress through `on_update`.
///
/// Returns the confirmed hosts. `username` is used for the stage-two identity
/// check and matches whatever the app is configured to connect as.
pub async fn scan<F>(network: LocalNetwork, port: u16, username: &str, mut on_update: F) -> Vec<String>
where
    F: FnMut(DiscoveryUpdate),
{
    let mut queue: VecDeque<String> = network.hosts().into();
    let total = queue.len();
    let mut done = 0;
    let mut found = Vec::new();

    tracing::info!(network = %network.label(), total, "sweeping for the device");

    // A sliding window rather than fixed chunks: a chunked sweep runs at the
    // speed of the slowest host in each chunk, and stage two on a real hit
    // takes seconds — enough to stall an entire batch of otherwise instant
    // probes behind it.
    let mut set = JoinSet::new();
    let spawn_next = |set: &mut JoinSet<_>, queue: &mut VecDeque<String>| {
        if let Some(host) = queue.pop_front() {
            let username = username.to_string();
            set.spawn(async move { (host.clone(), probe(&host, port, &username).await) });
        }
    };

    for _ in 0..CONCURRENCY {
        spawn_next(&mut set, &mut queue);
    }

    while let Some(result) = set.join_next().await {
        done += 1;
        on_update(DiscoveryUpdate::Progress { done, total });
        spawn_next(&mut set, &mut queue);

        // A panicking probe is reported as "not a device" rather than
        // aborting the whole sweep — one bad host shouldn't cost the
        // other 253.
        match result {
            Ok((host, Probe::Device)) => {
                on_update(DiscoveryUpdate::Found { host: host.clone() });
                found.push(host);
            }
            Ok((host, Probe::NotDevice(reason))) => {
                tracing::info!(%host, %reason, "host answered on the SSH port but isn't the handheld");
                on_update(DiscoveryUpdate::Rejected { host, reason });
            }
            Ok((_, Probe::Silent)) => {}
            Err(err) => tracing::warn!(%err, "a probe task failed"),
        }
    }

    tracing::info!(?found, "sweep finished");
    on_update(DiscoveryUpdate::Done);
    found
}

/// The outcome of probing one host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    /// Nothing listening on the SSH port.
    Silent,
    /// Listening, and it is the handheld.
    Device,
    /// Listening, but stage two rejected it — with the reason, so the UI can
    /// distinguish "a NAS" from "the handheld refused our credentials".
    NotDevice(String),
}

/// Two-stage check for a single host: is the SSH port open, and if so, is
/// there an SD card behind it?
async fn probe(host: &str, port: u16, username: &str) -> Probe {
    if !port_open(host, port).await {
        return Probe::Silent;
    }
    tracing::debug!(%host, "port open, checking for /mnt/SDCARD");
    identify(host, port, username).await
}

async fn port_open(host: &str, port: u16) -> bool {
    let Ok(addr) = format!("{host}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    matches!(tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(addr)).await, Ok(Ok(_)))
}

/// Confirms a listening host is actually the handheld by connecting with the
/// device's own auth quirks and looking for the SD card mount.
async fn identify(host: &str, port: u16, username: &str) -> Probe {
    match tokio::time::timeout(IDENTIFY_TIMEOUT, check_sdcard(host, port, username)).await {
        Ok(Ok(())) => Probe::Device,
        Ok(Err(reason)) => Probe::NotDevice(reason),
        Err(_) => Probe::NotDevice("timed out during the SSH handshake".into()),
    }
}

async fn check_sdcard(host: &str, port: u16, username: &str) -> Result<(), String> {
    let device =
        Device::connect(host, port, username).await.map_err(|err| format!("SSH connect failed: {err}"))?;
    let out = device
        .exec("test -d /mnt/SDCARD && echo emuhub-ok")
        .await
        .map_err(|err| format!("SSH connected but the check command failed: {err}"))?;
    if out.contains("emuhub-ok") {
        Ok(())
    } else {
        Err("SSH works but there is no /mnt/SDCARD".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(a: u8, b: u8, c: u8, d: u8, prefix_len: u8) -> LocalNetwork {
        LocalNetwork { local: Ipv4Addr::new(a, b, c, d), prefix_len }
    }

    #[test]
    fn subnet_prefix_drops_only_the_host_octet() {
        assert_eq!(subnet_prefix_of(Ipv4Addr::new(192, 168, 1, 47)), "192.168.1");
        assert_eq!(subnet_prefix_of(Ipv4Addr::new(10, 0, 0, 1)), "10.0.0");
    }

    #[test]
    fn a_24_covers_1_to_254_and_excludes_network_and_broadcast() {
        let hosts = net(192, 168, 1, 47, 24).hosts();
        // 254 hosts minus ourselves.
        assert_eq!(hosts.len(), 253);
        assert_eq!(hosts.first().unwrap(), "192.168.1.1");
        assert_eq!(hosts.last().unwrap(), "192.168.1.254");
        assert!(!hosts.contains(&"192.168.1.0".to_string()), ".0 is the network address, not a host");
        assert!(!hosts.contains(&"192.168.1.255".to_string()), ".255 is the broadcast address");
        assert!(!hosts.contains(&"192.168.1.47".to_string()), "we are not the handheld");
    }

    #[test]
    fn a_22_sweeps_all_four_of_its_24s() {
        // The bug this exists to prevent: an eero-style /22 whose leases run
        // past the first /24, where a handheld at .70.x was never probed.
        let hosts = net(192, 168, 68, 59, 22).hosts();
        assert!(hosts.contains(&"192.168.68.55".to_string()));
        assert!(hosts.contains(&"192.168.69.10".to_string()));
        assert!(hosts.contains(&"192.168.70.200".to_string()));
        assert!(hosts.contains(&"192.168.71.254".to_string()));
        assert!(!hosts.contains(&"192.168.68.0".to_string()), "network address");
        assert!(!hosts.contains(&"192.168.71.255".to_string()), "broadcast address");
        assert_eq!(hosts.len(), 1022 - 1, "1022 hosts in a /22, minus ourselves");
    }

    #[test]
    fn network_and_broadcast_come_from_the_prefix_length() {
        let n = net(192, 168, 68, 59, 22);
        assert_eq!(n.network(), Ipv4Addr::new(192, 168, 68, 0));
        assert_eq!(n.broadcast(), Ipv4Addr::new(192, 168, 71, 255));
        assert_eq!(n.label(), "192.168.68.0/22");
    }

    #[test]
    fn an_oversized_network_is_clamped_rather_than_swept_for_minutes() {
        // A /16 is 65534 hosts — a brute sweep there is the wrong tool, so
        // it narrows to the widest range under the ceiling around us.
        let n = net(172, 18, 4, 9, 16);
        assert_eq!(n.label(), "172.18.4.0/22");
        assert_eq!(n.hosts().len(), 1022 - 1);
        assert!(n.hosts().contains(&"172.18.4.1".to_string()));
        // The clamped range's own network/broadcast are what get skipped —
        // not the /16's, which aren't inside the swept range at all.
        assert!(!n.hosts().contains(&"172.18.4.0".to_string()));
        assert!(!n.hosts().contains(&"172.18.7.255".to_string()));
    }

    #[test]
    fn a_22_is_within_the_sweep_ceiling() {
        assert!(net(10, 0, 0, 5, 22).hosts().len() < MAX_HOSTS);
        assert_eq!(net(10, 0, 0, 5, 22).label(), "10.0.0.0/22");
    }

    #[test]
    fn every_generated_host_parses_as_an_address() {
        for host in net(10, 0, 0, 1, 24).hosts() {
            assert!(host.parse::<Ipv4Addr>().is_ok(), "{host} is not a valid address");
        }
    }

    #[test]
    fn a_degenerate_prefix_length_does_not_panic() {
        // Nothing sane hands out a /0, but a shift of 32 would overflow.
        assert_eq!(net(10, 0, 0, 1, 0).hosts().len(), 1022 - 1, "clamped to the ceiling");
    }

    #[tokio::test]
    async fn a_closed_port_is_not_open() {
        // Port 1 on localhost: reserved, and nothing in this sandbox binds it.
        assert!(!port_open("127.0.0.1", 1).await);
    }

    #[tokio::test]
    async fn an_unparseable_host_is_rejected_rather_than_resolved() {
        // Only literal addresses are swept, so a name must not trigger DNS.
        assert!(!port_open("not-an-address", 22).await);
    }

    #[tokio::test]
    async fn a_silent_host_probes_as_silent_rather_than_rejected() {
        assert_eq!(probe("127.0.0.1", 1, "root").await, Probe::Silent);
    }
}
