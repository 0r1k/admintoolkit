//! Curated catalog of Linux kernel tunables: sysctl keys, a handful of
//! sysfs-backed knobs (CPU governor, I/O scheduler, transparent hugepage),
//! and `/etc/security/limits.d` entries. Every entry carries a plain-English
//! description, the *why* (the actual best-practice rationale, including
//! honest tradeoffs — not just "set this to go fast"), and per-scenario
//! recommended values. This is reference data, not logic — `engine.rs` is
//! what actually reads/writes any of it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Network,
    Memory,
    FsLimits,
    CpuScheduler,
    Security,
}

impl Category {
    pub const ALL: [Category; 5] = [Category::Network, Category::Memory, Category::FsLimits, Category::CpuScheduler, Category::Security];

    pub fn label(self) -> &'static str {
        match self {
            Category::Network => "Network",
            Category::Memory => "Memory / VM",
            Category::FsLimits => "Filesystem & Limits",
            Category::CpuScheduler => "CPU & Scheduler",
            Category::Security => "Security Hardening",
        }
    }
}

/// A usage scenario. Picking one in the Catalog tab stages every tunable
/// that has a recommendation for it in one motion; individual values can
/// still be hand-edited afterward, so a profile is a starting point, not a
/// straitjacket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Profile {
    Desktop,
    Traffic,
    Database,
    Gaming,
    AiCompute,
    Security,
}

impl Profile {
    pub const ALL: [Profile; 6] =
        [Profile::Desktop, Profile::Traffic, Profile::Database, Profile::Gaming, Profile::AiCompute, Profile::Security];

    pub fn label(self) -> &'static str {
        match self {
            Profile::Desktop => "Desktop",
            Profile::Traffic => "Network / Traffic / Web Server",
            Profile::Database => "Database / Big Data",
            Profile::Gaming => "Gaming Server",
            Profile::AiCompute => "AI / Compute Server",
            Profile::Security => "Security Hardening",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Safe,
    Caution,
    Advanced,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Safe => "Safe",
            Risk::Caution => "Caution",
            Risk::Advanced => "Advanced",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// A plain `sysctl` key, e.g. `net.core.somaxconn`.
    Sysctl,
    /// A sysfs attribute. The `&str` is a shell glob that may match more
    /// than one device (e.g. one `scaling_governor` file per CPU core, one
    /// `scheduler` file per block device) — applying sets every match to
    /// the same value, reading shows the first match as representative.
    /// Persisting these has no `sysctl.d`-equivalent, so `engine.rs`
    /// installs a small systemd oneshot unit that replays them at boot.
    Sysfs(&'static str),
    /// A `soft`/`hard` pair in `/etc/security/limits.d` — PAM-enforced at
    /// login, so there is no "live" value to change at runtime; applying
    /// this kind always means writing the file.
    Limits { domain: &'static str, item: &'static str },
}

#[derive(Debug, Clone, Copy)]
pub struct ProfileValue {
    pub profile: Profile,
    pub value: &'static str,
    /// Why *this* value for *this* profile specifically, when it differs
    /// from the general `why`.
    pub note: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct Tunable {
    /// Canonical id — the sysctl key, or a synthetic dotted key for
    /// `Sysfs`/`Limits` kinds (e.g. `cpu.governor`, `limits.nofile`).
    pub key: &'static str,
    pub kind: Kind,
    pub category: Category,
    pub title: &'static str,
    /// What it does, in plain terms.
    pub description: &'static str,
    /// The best-practice rationale — including tradeoffs, not just upsides.
    pub why: &'static str,
    pub risk: Risk,
    /// The typical out-of-the-box value, for orientation only — the actual
    /// current value is always read live from the target.
    pub default_hint: &'static str,
    pub profiles: &'static [ProfileValue],
}

impl Tunable {
    pub fn recommended_for(&self, profile: Profile) -> Option<&'static ProfileValue> {
        self.profiles.iter().find(|pv| pv.profile == profile)
    }
}

pub fn by_key(key: &str) -> Option<&'static Tunable> {
    CATALOG.iter().find(|t| t.key == key)
}

macro_rules! pv {
    ($profile:expr, $value:expr, $note:expr) => {
        ProfileValue { profile: $profile, value: $value, note: $note }
    };
}

use Kind::*;
use Profile::*;
use Risk::*;

pub static CATALOG: &[Tunable] = &[
    // ── Network ──────────────────────────────────────────────────────────
    Tunable {
        key: "net.core.somaxconn",
        kind: Sysctl,
        category: Category::Network,
        title: "Listen backlog (somaxconn)",
        description: "Max length of the queue of pending (not-yet-accepted) TCP connections per listening socket.",
        why: "The default is far too small for a server that accepts bursts of connections faster than the app can call accept(). Raising it avoids the kernel silently dropping SYNs under load. It only helps if the app's own listen() backlog argument is raised to match — nginx/redis/postgres all default to using a large one already.",
        risk: Safe,
        default_hint: "4096 (kernel default varies 128-4096 by distro)",
        profiles: &[
            pv!(Traffic, "65535", "High-concurrency web/proxy servers see bursts on every deploy/restart of upstreams."),
            pv!(Gaming, "32768", "Lobby/matchmaking servers see connection bursts at patch-launch/event-start times."),
            pv!(Database, "4096", "DB connections are usually pooled app-side, so bursts are smaller than for a public-facing server."),
        ],
    },
    Tunable {
        key: "net.core.netdev_max_backlog",
        kind: Sysctl,
        category: Category::Network,
        title: "NIC receive queue backlog",
        description: "Max packets queued for the kernel to process when a NIC receives faster than the kernel can drain the queue.",
        why: "On high-packet-rate NICs (10G+, or many small packets like game traffic) the default queue is too shallow and packets get dropped before they're even seen by iptables/the socket layer.",
        risk: Safe,
        default_hint: "1000",
        profiles: &[
            pv!(Traffic, "65536", "High packets-per-second web/proxy/CDN traffic."),
            pv!(Gaming, "65536", "Many small UDP packets (voice, position updates) are exactly the pattern this protects against."),
        ],
    },
    Tunable {
        key: "net.ipv4.tcp_max_syn_backlog",
        kind: Sysctl,
        category: Category::Network,
        title: "SYN backlog",
        description: "Max number of half-open (SYN received, handshake not complete) connections tracked per listening socket.",
        why: "Works together with somaxconn; a shallow SYN backlog is the classic cause of connection resets during a traffic spike. Safe to raise as long as tcp_syncookies stays enabled as the overflow fallback.",
        risk: Safe,
        default_hint: "128-512",
        profiles: &[pv!(Traffic, "65535", "Matches a raised somaxconn — otherwise the SYN queue becomes the new bottleneck instead.")],
    },
    Tunable {
        key: "net.ipv4.tcp_fin_timeout",
        kind: Sysctl,
        category: Category::Network,
        title: "FIN-WAIT-2 timeout",
        description: "How long (seconds) a socket stays in FIN-WAIT-2 after the local side closes, before the kernel gives up.",
        why: "The default is conservative for servers that open and close a lot of short-lived connections (HTTP/1.1 without keep-alive, health checks). Lowering it frees the socket/port faster; too low can drop a legitimately slow far end.",
        risk: Safe,
        default_hint: "60",
        profiles: &[pv!(Traffic, "15", "High connection churn — 60s of lingering sockets adds up fast under load."), pv!(Gaming, "10", "Short match-server connections benefit from faster socket reuse.")],
    },
    Tunable {
        key: "net.ipv4.tcp_tw_reuse",
        kind: Sysctl,
        category: Category::Network,
        title: "Reuse TIME_WAIT sockets",
        description: "Allows the kernel to reuse a socket in TIME_WAIT state for a new outgoing connection when it's safe to do so (per RFC 1323 timestamps).",
        why: "Frees up the local ephemeral port range faster under high connection turnover. This is the safe cousin of the old tcp_tw_recycle (removed from the kernel — it broke NAT'd clients); tw_reuse only affects outgoing connections and has no such hazard.",
        risk: Safe,
        default_hint: "0 or 2 (distro-dependent)",
        profiles: &[pv!(Traffic, "1", "Servers making lots of short-lived outbound connections (proxies, health checkers) exhaust the ephemeral port range without this.")],
    },
    Tunable {
        key: "net.ipv4.tcp_slow_start_after_idle",
        kind: Sysctl,
        category: Category::Network,
        title: "Reset congestion window after idle",
        description: "When enabled (default), a connection's congestion window resets to slow-start after a period of no traffic.",
        why: "This punishes exactly the pattern of bursty-but-persistent connections (game servers, long-lived streaming/replication links): traffic resumes at slow-start speed even though the connection already proved it could sustain more.",
        risk: Safe,
        default_hint: "1 (enabled)",
        profiles: &[pv!(Gaming, "0", "Persistent player connections shouldn't be throttled back to slow-start between bursts of activity."), pv!(Database, "0", "Replication/backup links are bursty but long-lived.")],
    },
    Tunable {
        key: "net.ipv4.tcp_congestion_control",
        kind: Sysctl,
        category: Category::Network,
        title: "TCP congestion control algorithm",
        description: "Which algorithm governs how aggressively TCP ramps up throughput and reacts to loss.",
        why: "BBR (Google) models the actual bottleneck bandwidth/RTT instead of reacting to loss like CUBIC does, and noticeably improves throughput on lossy or long-haul links. Requires kernel >= 4.9 with the tcp_bbr module available — if `modprobe tcp_bbr` fails, this write will fail too, which is informative rather than a bug.",
        risk: Caution,
        default_hint: "cubic",
        profiles: &[
            pv!(Traffic, "bbr", "Best-documented win: higher throughput on the public internet's typically-lossy paths."),
            pv!(Gaming, "bbr", "Lower induced latency under load than loss-based algorithms — matters for real-time traffic."),
            pv!(AiCompute, "bbr", "Large distributed-training transfers benefit from BBR's throughput on long-haul/high-bandwidth links."),
        ],
    },
    Tunable {
        key: "net.core.default_qdisc",
        kind: Sysctl,
        category: Category::Network,
        title: "Default queueing discipline",
        description: "The packet scheduler applied to network interfaces by default.",
        why: "`fq` (fair queueing) is the qdisc BBR was designed to be paired with — using them together is the standard recommendation, not an optional extra.",
        risk: Safe,
        default_hint: "pfifo_fast",
        profiles: &[pv!(Traffic, "fq", "Pairs with BBR."), pv!(Gaming, "fq", "Pairs with BBR."), pv!(AiCompute, "fq", "Pairs with BBR.")],
    },
    Tunable {
        key: "net.core.rmem_max",
        kind: Sysctl,
        category: Category::Network,
        title: "Max socket receive buffer",
        description: "Upper bound (bytes) any single socket's receive buffer can be set to, including via SO_RCVBUF.",
        why: "High-throughput / high-latency (long fat network) links need a much bigger buffer than the default to keep the pipe full — the classic bandwidth-delay-product argument. Wasted on low-bandwidth or purely-local traffic, hence not in every profile.",
        risk: Safe,
        default_hint: "212992",
        profiles: &[pv!(Traffic, "134217728", "128MB — sized for high-bandwidth WAN links."), pv!(AiCompute, "134217728", "Distributed training/data-loading over fast networks.")],
    },
    Tunable {
        key: "net.core.wmem_max",
        kind: Sysctl,
        category: Category::Network,
        title: "Max socket send buffer",
        description: "Upper bound (bytes) any single socket's send buffer can be set to.",
        why: "Same reasoning as rmem_max, for the send side.",
        risk: Safe,
        default_hint: "212992",
        profiles: &[pv!(Traffic, "134217728", "Matches rmem_max."), pv!(AiCompute, "134217728", "Matches rmem_max.")],
    },
    Tunable {
        key: "net.ipv4.tcp_rmem",
        kind: Sysctl,
        category: Category::Network,
        title: "TCP receive buffer (min / default / max)",
        description: "Three values: the minimum, initial default, and maximum size of a TCP socket's auto-tuned receive buffer.",
        why: "Auto-tuning only ever grows up to this ceiling, so it needs raising alongside rmem_max to actually take advantage of it.",
        risk: Safe,
        default_hint: "4096 87380 6291456",
        profiles: &[pv!(Traffic, "4096 87380 134217728", "Ceiling matches rmem_max."), pv!(AiCompute, "4096 87380 134217728", "Ceiling matches rmem_max.")],
    },
    Tunable {
        key: "net.ipv4.tcp_wmem",
        kind: Sysctl,
        category: Category::Network,
        title: "TCP send buffer (min / default / max)",
        description: "Three values: the minimum, initial default, and maximum size of a TCP socket's auto-tuned send buffer.",
        why: "Same reasoning as tcp_rmem, for the send side.",
        risk: Safe,
        default_hint: "4096 16384 4194304",
        profiles: &[pv!(Traffic, "4096 65536 134217728", "Ceiling matches wmem_max."), pv!(AiCompute, "4096 65536 134217728", "Ceiling matches wmem_max.")],
    },
    Tunable {
        key: "net.ipv4.ip_local_port_range",
        kind: Sysctl,
        category: Category::Network,
        title: "Ephemeral port range",
        description: "The range of local ports the kernel picks from for outgoing connections that don't bind a specific port.",
        why: "A server that makes lots of outbound connections (reverse proxy, load balancer, anything fronting upstreams) can exhaust the default range under load, causing connect() to fail with EADDRNOTAVAIL.",
        risk: Safe,
        default_hint: "32768 60999",
        profiles: &[pv!(Traffic, "1024 65535", "Widens the pool for proxies/load balancers making many outbound connections.")],
    },
    Tunable {
        key: "net.ipv4.tcp_fastopen",
        kind: Sysctl,
        category: Category::Network,
        title: "TCP Fast Open",
        description: "Lets data ride along with the SYN packet, skipping a full round trip on repeat connections to the same server.",
        why: "Cuts one RTT off every reconnect — meaningful for high-latency mobile clients. Value 3 enables it for both client and server roles; needs application support (most modern HTTP stacks have it) to actually be used.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Traffic, "3", "Web-facing servers with lots of repeat client connections benefit most.")],
    },
    Tunable {
        key: "net.ipv4.tcp_keepalive_time",
        kind: Sysctl,
        category: Category::Network,
        title: "TCP keepalive idle time",
        description: "Seconds of idleness before the kernel starts sending keepalive probes on a connection that asked for them.",
        why: "The 2-hour default is far too slow to notice a dead peer (crashed client, yanked cable, NAT timeout) in near-real-time. Lowering it detects dead connections faster at the cost of a small amount of extra idle traffic.",
        risk: Safe,
        default_hint: "7200",
        profiles: &[pv!(Gaming, "300", "Detect disconnected players quickly instead of holding a slot for a dead connection."), pv!(Traffic, "600", "Faster dead-upstream/dead-client detection behind load balancers.")],
    },
    Tunable {
        key: "net.netfilter.nf_conntrack_max",
        kind: Sysctl,
        category: Category::Network,
        title: "Max tracked connections (conntrack)",
        description: "Ceiling on how many connections the kernel's connection tracker (used by iptables/nftables NAT and stateful rules) will track simultaneously.",
        why: "Once this fills up, new connections are silently dropped — a common, hard-to-diagnose cause of intermittent connectivity failures on busy NAT/firewall boxes. Only present if the nf_conntrack module is loaded (anything using iptables/nftables connection state, docker, etc.) — the write will fail harmlessly if it isn't.",
        risk: Caution,
        default_hint: "65536 (module-dependent)",
        profiles: &[pv!(Traffic, "1048576", "High-connection-count servers behind NAT/stateful firewall rules (very common with Docker/K8s nodes).")],
    },
    Tunable {
        key: "net.ipv4.conf.all.rp_filter",
        kind: Sysctl,
        category: Category::Security,
        title: "Reverse path filtering",
        description: "Drops incoming packets whose source address wouldn't be routed back out the interface they arrived on — basic anti-spoofing.",
        why: "Strict mode (1) blocks classic source-address-spoofing attacks. Only a problem for asymmetric-routing setups (multi-homed boxes, some anycast/BGP configs) where legitimate return traffic doesn't take the same path — not a concern for a typical single-uplink server.",
        risk: Safe,
        default_hint: "distro-dependent (0, 1, or 2)",
        profiles: &[pv!(Security, "1", "Standard anti-spoofing hardening for single-homed hosts.")],
    },
    Tunable {
        key: "net.ipv4.tcp_syncookies",
        kind: Sysctl,
        category: Category::Security,
        title: "SYN cookies",
        description: "Falls back to a stateless cryptographic handshake when the SYN backlog is full, instead of dropping new connections.",
        why: "The standard kernel-level defense against SYN-flood DoS. Already 1 on essentially every modern distro; this entry exists to make sure it stays that way and to document why it must never be turned off.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Security, "1", "Never disable — this is the safety net for every other backlog tuning above.")],
    },
    Tunable {
        key: "net.ipv4.conf.all.accept_redirects",
        kind: Sysctl,
        category: Category::Security,
        title: "Accept ICMP redirects",
        description: "Whether the kernel updates its routing table in response to ICMP redirect messages from other hosts on the network.",
        why: "A host that isn't itself a router has no legitimate reason to accept redirects, and accepting them lets an on-path attacker quietly repoint your traffic.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Security, "0", "Only routers legitimately need this; a server or desktop doesn't.")],
    },
    Tunable {
        key: "net.ipv4.conf.all.send_redirects",
        kind: Sysctl,
        category: Category::Security,
        title: "Send ICMP redirects",
        description: "Whether the kernel sends ICMP redirects to hosts it thinks are using a suboptimal route.",
        why: "Only relevant if this box is acting as a router between two networks — sending redirects otherwise makes no sense and gives an attacker one more thing to abuse.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Security, "0", "Not a router — disable.")],
    },
    Tunable {
        key: "net.ipv4.conf.all.log_martians",
        kind: Sysctl,
        category: Category::Security,
        title: "Log martian packets",
        description: "Logs packets with impossible source addresses (e.g. claiming to be from a private range on a public interface).",
        why: "Cheap visibility into spoofing attempts and misconfigured neighbors, at the cost of a bit of log volume on a noisy network.",
        risk: Safe,
        default_hint: "0",
        profiles: &[pv!(Security, "1", "Low-cost detection signal for spoofed traffic.")],
    },

    // ── Memory / VM ──────────────────────────────────────────────────────
    Tunable {
        key: "vm.swappiness",
        kind: Sysctl,
        category: Category::Memory,
        title: "Swappiness",
        description: "How aggressively (0-100) the kernel swaps anonymous memory out to disk versus reclaiming page cache first.",
        why: "The default of 60 is tuned for general-purpose desktop use. A database wants its hot working set to stay in RAM at almost any cost — swapping out a page mid-query is a latency cliff. A gaming/AI server has the same interest in not stuttering.",
        risk: Safe,
        default_hint: "60",
        profiles: &[
            pv!(Database, "1", "Only swap as an absolute last resort before OOM — a swapped page is a query-killing latency spike."),
            pv!(Gaming, "10", "Avoid swap-induced stutter under memory pressure."),
            pv!(AiCompute, "10", "Training/inference workloads want their working set pinned in RAM."),
            pv!(Traffic, "10", "Request latency matters more than reclaiming a little extra cache."),
            pv!(Desktop, "20", "Modern desktops with plenty of RAM rarely need aggressive swapping; a lower value feels snappier under load without disabling swap's safety net."),
        ],
    },
    Tunable {
        key: "vm.dirty_ratio",
        kind: Sysctl,
        category: Category::Memory,
        title: "Dirty page ratio (hard limit)",
        description: "Percentage of RAM that can be filled with unwritten (dirty) page-cache pages before a writing process is forced to block and flush synchronously.",
        why: "The default lets a lot of dirty data pile up before applying backpressure, which then shows up as a sudden multi-second write stall exactly when the flush finally happens — bad for anything latency-sensitive doing its own writes (databases especially).",
        risk: Caution,
        default_hint: "20",
        profiles: &[pv!(Database, "10", "Flush sooner, in smaller increments, instead of one big stall."), pv!(Traffic, "15", "Smoother write-back under sustained logging/traffic.")],
    },
    Tunable {
        key: "vm.dirty_background_ratio",
        kind: Sysctl,
        category: Category::Memory,
        title: "Dirty page ratio (background flush)",
        description: "Percentage of RAM dirty before the kernel starts flushing in the background, asynchronously, before hitting the hard `dirty_ratio` limit.",
        why: "Should stay comfortably below dirty_ratio so background flushing has already been chipping away well before the hard synchronous stall would kick in.",
        risk: Safe,
        default_hint: "10",
        profiles: &[pv!(Database, "5", "Start background flushing earlier, paired with the lower dirty_ratio above.")],
    },
    Tunable {
        key: "vm.dirty_expire_centisecs",
        kind: Sysctl,
        category: Category::Memory,
        title: "Dirty page max age",
        description: "How long (centiseconds) a dirty page is allowed to sit before it's eligible for write-back, regardless of the ratio thresholds.",
        why: "Bounds how much unwritten data you could lose on a crash, and smooths out write-back instead of it being purely ratio-triggered.",
        risk: Safe,
        default_hint: "3000 (30s)",
        profiles: &[pv!(Database, "1500", "15s — tighter crash-consistency window alongside the lower dirty ratios above.")],
    },
    Tunable {
        key: "vm.overcommit_memory",
        kind: Sysctl,
        category: Category::Memory,
        title: "Memory overcommit policy",
        description: "0 = heuristic overcommit (default), 1 = always allow, 2 = strict accounting against swap+a fraction of RAM.",
        why: "Redis's own docs recommend 1: without it, a `fork()` for a background save (BGSAVE/AOF rewrite) can fail under memory pressure even though the child only needs copy-on-write pages, not a full duplicate allocation. Changes overcommit behavior for every process on the box, not just one service, so it's worth understanding before flipping.",
        risk: Caution,
        default_hint: "0",
        profiles: &[pv!(Database, "1", "Prevents fork()-for-background-save failures under memory pressure (Redis, and similar fork-to-persist designs)."), pv!(AiCompute, "1", "Large one-shot allocations (model weights, batch buffers) are less likely to be heuristically rejected.")],
    },
    Tunable {
        key: "vm.max_map_count",
        kind: Sysctl,
        category: Category::Memory,
        title: "Max memory map areas per process",
        description: "Ceiling on the number of distinct virtual memory mapping areas (mmap regions) a single process may have.",
        why: "Elasticsearch/OpenSearch refuse to start below 262144 (each Lucene index segment is its own mmap). ML frameworks that mmap many small files/tensors, or use lots of shared-memory segments, hit the same wall.",
        risk: Safe,
        default_hint: "65530",
        profiles: &[pv!(Database, "262144", "Required minimum for Elasticsearch/OpenSearch; harmless headroom for anything else."), pv!(AiCompute, "262144", "Frameworks that mmap many small files/tensors can otherwise fail with ENOMEM despite free RAM.")],
    },
    Tunable {
        key: "vm.vfs_cache_pressure",
        kind: Sysctl,
        category: Category::Memory,
        title: "VFS cache reclaim pressure",
        description: "How eagerly the kernel reclaims dentry/inode cache versus other reclaimable memory. Lower = kept longer.",
        why: "Workloads that touch a large number of files (many small tables/segments, log shipping, big repos) benefit from the kernel holding onto directory/inode metadata longer instead of re-walking disk for it constantly.",
        risk: Safe,
        default_hint: "100",
        profiles: &[pv!(Database, "50", "Large numbers of data files/segments benefit from metadata staying cached."), pv!(Traffic, "50", "Many small static files (assets, logs) benefit similarly.")],
    },
    Tunable {
        key: "vm.min_free_kbytes",
        kind: Sysctl,
        category: Category::Memory,
        title: "Reserved free memory floor",
        description: "How much RAM the kernel always keeps free as a buffer, so allocations under memory pressure don't have to block on synchronous reclaim.",
        why: "The kernel's auto-computed default scales with RAM but can still be too low on large-memory boxes under bursty allocation patterns, showing up as latency spikes exactly when memory pressure hits. The value below is a reasonable starting floor for a large-memory server — scale it up further on boxes with very large RAM (rule of thumb: roughly 1-3% of total RAM).",
        risk: Caution,
        default_hint: "auto-computed from RAM",
        profiles: &[pv!(Database, "1048576", "1GB floor — a starting point for large-memory DB servers; adjust for actual RAM size.")],
    },
    Tunable {
        key: "kernel.numa_balancing",
        kind: Sysctl,
        category: Category::Memory,
        title: "Automatic NUMA balancing",
        description: "Lets the kernel automatically migrate memory pages and tasks between NUMA nodes to improve locality.",
        why: "Automatic balancing is a reasonable general default, but for workloads that already pin threads/memory explicitly to NUMA nodes (many DB engines, distributed training frameworks), the kernel's own migration can fight the application's placement decisions and add jitter instead of removing it.",
        risk: Caution,
        default_hint: "1",
        profiles: &[pv!(Database, "0", "Disable if the DB engine does its own NUMA pinning (check its docs first)."), pv!(AiCompute, "0", "Disable when the training/inference framework does its own NUMA/GPU-affinity pinning.")],
    },

    // ── Filesystem & Limits ──────────────────────────────────────────────
    Tunable {
        key: "fs.file-max",
        kind: Sysctl,
        category: Category::FsLimits,
        title: "System-wide max open files",
        description: "The ceiling on how many file descriptors can be open across the whole system at once (not per-process — see the nofile ulimit below for that).",
        why: "Per-process ulimits are meaningless if the system-wide ceiling is hit first. Needs raising alongside the nofile limit, not instead of it.",
        risk: Safe,
        default_hint: "distro/RAM-dependent",
        profiles: &[pv!(Traffic, "2097152", "High connection-count servers."), pv!(Database, "2097152", "Many open table/segment files plus client connections."), pv!(Gaming, "2097152", "Many concurrent player connections/sockets.")],
    },
    Tunable {
        key: "fs.inotify.max_user_watches",
        kind: Sysctl,
        category: Category::FsLimits,
        title: "Max inotify watches per user",
        description: "How many files a single user's processes can watch for changes via inotify (used by IDEs, file sync tools, log shippers, hot-reload dev servers).",
        why: "The classic 'ENOSPC: System limit for number of file watchers reached' error from any large IDE or dev server, or from log-shipping agents watching many files.",
        risk: Safe,
        default_hint: "8192-65536 (distro-dependent)",
        profiles: &[pv!(Desktop, "524288", "IDEs, file-watchers, and dev tooling routinely need more than the default."), pv!(Database, "524288", "Log-shipping/monitoring agents watching many files.")],
    },
    Tunable {
        key: "fs.inotify.max_user_instances",
        kind: Sysctl,
        category: Category::FsLimits,
        title: "Max inotify instances per user",
        description: "How many separate inotify instances (not watches — whole instances) a user's processes can create.",
        why: "Less commonly hit than max_user_watches, but some multi-process dev tooling (multiple containers/watchers per user) can bump into the low default.",
        risk: Safe,
        default_hint: "128",
        profiles: &[pv!(Desktop, "1024", "Multiple concurrent dev-tool/watch processes.")],
    },
    Tunable {
        key: "fs.aio-max-nr",
        kind: Sysctl,
        category: Category::FsLimits,
        title: "Max concurrent async I/O requests",
        description: "System-wide ceiling on outstanding Linux AIO requests (the io_submit()/libaio interface).",
        why: "MySQL/InnoDB (innodb_use_native_aio) and PostgreSQL can hit this under heavy concurrent I/O, failing with a cryptic 'Resource temporarily unavailable' rather than anything that obviously points at this setting.",
        risk: Safe,
        default_hint: "65536",
        profiles: &[pv!(Database, "1048576", "Heavy concurrent I/O engines (InnoDB native AIO) need real headroom here.")],
    },
    Tunable {
        key: "limits.nofile",
        kind: Limits { domain: "*", item: "nofile" },
        category: Category::FsLimits,
        title: "Open file descriptor limit (ulimit -n)",
        description: "Per-user soft/hard cap on simultaneously open file descriptors — sockets count as file descriptors too.",
        why: "The distro default (often 1024) is exhausted almost immediately by any server handling real concurrency — every open connection, log file, and DB handle counts against it. PAM-enforced, so it only takes effect for new login sessions after saving (existing sessions/services keep their current limit until restarted).",
        risk: Safe,
        default_hint: "1024 soft (distro-dependent)",
        profiles: &[
            pv!(Traffic, "1048576", "Every connection is a file descriptor; this is the single most common 'too many open files' fix."),
            pv!(Database, "1048576", "Connections plus open table/segment files add up fast."),
            pv!(Gaming, "1048576", "Every connected player is a socket."),
        ],
    },
    Tunable {
        key: "limits.nproc",
        kind: Limits { domain: "*", item: "nproc" },
        category: Category::FsLimits,
        title: "Max processes/threads per user (ulimit -u)",
        description: "Per-user cap on simultaneously running processes and threads.",
        why: "Thread-heavy servers (connection-per-thread pools, JVM-based databases) can hit this well before any other resource limit, failing to spawn new threads under load with a confusing error.",
        risk: Safe,
        default_hint: "distro-dependent",
        profiles: &[pv!(Database, "65535", "Thread-per-connection or JVM-based engines can spawn a lot of threads."), pv!(Traffic, "65535", "Worker-process/thread pools under high concurrency.")],
    },

    // ── CPU & Scheduler ──────────────────────────────────────────────────
    Tunable {
        key: "cpu.governor",
        kind: Sysfs("/sys/devices/system/cpu/cpu[0-9]*/cpufreq/scaling_governor"),
        category: Category::CpuScheduler,
        title: "CPU frequency governor",
        description: "Controls how aggressively each CPU core scales its clock speed with load. `performance` pins cores at their max frequency; `powersave`/`schedutil` scale down when idle to save power.",
        why: "Frequency-scaling transitions add latency jitter — bad for anything latency-sensitive (game tick rate, DB query tail latency, training step time). `performance` trades power/heat for consistent low-latency response. Only has an effect where the CPU/driver exposes cpufreq scaling at all (not all cloud VMs do).",
        risk: Caution,
        default_hint: "powersave or schedutil (distro-dependent)",
        profiles: &[
            pv!(Gaming, "performance", "Eliminates frequency-scaling latency jitter — the single biggest CPU-side win for tick-rate consistency."),
            pv!(Database, "performance", "Query tail latency benefits from not waiting on a frequency ramp-up."),
            pv!(AiCompute, "performance", "Training/inference throughput wants sustained max clock, not power savings."),
        ],
    },
    Tunable {
        key: "io.scheduler",
        kind: Sysfs("/sys/block/[a-z]*/queue/scheduler"),
        category: Category::CpuScheduler,
        title: "Block I/O scheduler",
        description: "Which algorithm orders and merges pending disk I/O requests per block device.",
        why: "`mq-deadline` is a safe general-purpose choice that bounds request latency better than the default on most SSD/HDD setups. NVMe devices in particular are often better served by `none` (let the device's own deep queue handle ordering) — this is genuinely device-dependent, so treat the recommendation below as a starting point to verify against your actual disks, not a universal answer.",
        risk: Advanced,
        default_hint: "mq-deadline, none, or bfq (device/distro-dependent)",
        profiles: &[pv!(Database, "mq-deadline", "Bounded latency is usually what a DB wants; re-check against `none` specifically if the storage is NVMe.")],
    },
    Tunable {
        key: "kernel.sched_autogroup_enabled",
        kind: Sysctl,
        category: Category::CpuScheduler,
        title: "Scheduler autogrouping",
        description: "Automatically groups tasks by session (roughly: by terminal/login session) for fairer CPU-time distribution between them.",
        why: "Designed for desktop interactivity (so a `make -j` build doesn't starve your terminal). On a dedicated server running one demanding workload, it adds scheduling overhead and can group unrelated processes in ways that hurt a single latency-sensitive service.",
        risk: Caution,
        default_hint: "1",
        profiles: &[pv!(Gaming, "0", "A dedicated game server doesn't benefit from desktop-session-fairness grouping — only adds scheduling overhead.")],
    },
    Tunable {
        key: "kernel.watchdog",
        kind: Sysctl,
        category: Category::CpuScheduler,
        title: "NMI watchdog",
        description: "Periodic hardware-timer interrupt used to detect kernel hangs (soft/hard lockups).",
        why: "Genuinely useful for catching kernel hangs, but the periodic NMI is itself a small, regular source of jitter — measurable on latency-sensitive workloads. Disabling trades away hang detection for a small amount of consistency; not something to disable on a box you can't otherwise monitor.",
        risk: Caution,
        default_hint: "1",
        profiles: &[pv!(Gaming, "0", "Removes a small periodic jitter source; only worth it if you have other health monitoring in place."), pv!(AiCompute, "0", "Long training runs are sensitive to any periodic interruption of sustained compute.")],
    },
    Tunable {
        key: "kernel.perf_event_paranoid",
        kind: Sysctl,
        category: Category::CpuScheduler,
        title: "Perf event access level",
        description: "How much unprivileged access non-root users have to CPU performance counters and tracing (`perf`, and profilers built on it).",
        why: "Profiling tools (perf, many GPU/ML profilers) need this lowered to work without root. Also widens what an unprivileged user can observe about other processes' behavior via timing side-channels, which is exactly why the hardened default exists — only lower it on boxes where the users running profilers are already trusted.",
        risk: Caution,
        default_hint: "2-4 (distro-dependent)",
        profiles: &[pv!(AiCompute, "1", "Lets profiling tools attach without root; keep root-only (2+) on multi-tenant or untrusted-user boxes.")],
    },

    // ── Security Hardening ───────────────────────────────────────────────
    Tunable {
        key: "kernel.kptr_restrict",
        kind: Sysctl,
        category: Category::Security,
        title: "Hide kernel pointers",
        description: "Restricts unprivileged reads of kernel memory addresses exposed via /proc (e.g. /proc/kallsyms).",
        why: "Kernel addresses are a building block for kernel-exploit development (defeating KASLR). Hiding them from unprivileged users removes that building block for free.",
        risk: Safe,
        default_hint: "0-1 (distro-dependent)",
        profiles: &[pv!(Security, "2", "Fully hides kernel pointers from unprivileged reads.")],
    },
    Tunable {
        key: "kernel.dmesg_restrict",
        kind: Sysctl,
        category: Category::Security,
        title: "Restrict dmesg to root",
        description: "Whether unprivileged users can read the kernel ring buffer (dmesg).",
        why: "The kernel log can leak addresses and details useful for exploitation, and is rarely something a non-root user legitimately needs.",
        risk: Safe,
        default_hint: "0",
        profiles: &[pv!(Security, "1", "Standard baseline hardening — root-only dmesg access.")],
    },
    Tunable {
        key: "kernel.yama.ptrace_scope",
        kind: Sysctl,
        category: Category::Security,
        title: "Restrict ptrace scope",
        description: "Limits which processes a user can attach to via ptrace (the mechanism behind gdb, strace, and some exploit techniques).",
        why: "Restricting ptrace to parent-child relationships (value 1) closes off a common privilege-escalation and credential-theft technique (attaching to another process of the same user to read its memory), at the cost of needing CAP_SYS_PTRACE for ad-hoc 'attach gdb to an unrelated running process' debugging.",
        risk: Caution,
        default_hint: "0",
        profiles: &[pv!(Security, "1", "Standard hardening; can require `sudo gdb -p PID` for debugging workflows that previously didn't need root.")],
    },
    Tunable {
        key: "kernel.sysrq",
        kind: Sysctl,
        category: Category::Security,
        title: "Magic SysRq key",
        description: "Enables the SysRq key combination for low-level emergency actions (force sync, remount read-only, reboot, kernel-level process kill).",
        why: "Genuinely useful for recovering a hung box without a hard power-cycle, but on a physically-accessible or console-exposed machine it's also a way to bypass normal access controls entirely. Whether that tradeoff is worth it depends entirely on how exposed the console is.",
        risk: Caution,
        default_hint: "1 or 176 (distro-dependent)",
        profiles: &[pv!(Security, "0", "Disables the emergency console escape hatch — only recommended if you have another reliable way to recover a hung system.")],
    },
    Tunable {
        key: "net.ipv4.tcp_timestamps",
        kind: Sysctl,
        category: Category::Security,
        title: "TCP timestamps",
        description: "Adds a timestamp to TCP packets, used for round-trip-time estimation and protecting against wrapped sequence numbers (PAWS).",
        why: "Older hardening guides recommend disabling this because a system's uptime can be inferred from the timestamp value. In practice this is a very minor information leak, and disabling it gives up real things: worse RTT estimation and loss of PAWS protection, which matters more on the high-bandwidth/long-fat-network links this same catalog tunes for elsewhere. Modern guidance (including the kernel's own maintainers) generally favors leaving it on — included here for completeness, not as a default recommendation.",
        risk: Advanced,
        default_hint: "1",
        profiles: &[pv!(Security, "0", "The traditional hardening-guide recommendation — weigh the minor info-leak reduction against the real performance/PAWS cost above before applying.")],
    },
    Tunable {
        key: "fs.protected_hardlinks",
        kind: Sysctl,
        category: Category::Security,
        title: "Restrict hardlink creation",
        description: "Prevents users from creating hardlinks to files they don't own in world-writable directories.",
        why: "Closes a classic local privilege-escalation/TOCTOU vector (hardlinking to a privileged file you don't own, e.g. a setuid binary, in a shared tmp directory). Already the default on essentially every current distro kernel.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Security, "1", "Confirms/restores the safe default.")],
    },
    Tunable {
        key: "fs.protected_symlinks",
        kind: Sysctl,
        category: Category::Security,
        title: "Restrict symlink following",
        description: "Prevents following a symlink you don't own in a world-writable sticky directory (like /tmp) under conditions that enable classic symlink-race attacks.",
        why: "Closes another classic /tmp-race privilege-escalation vector. Already the default on essentially every current distro kernel.",
        risk: Safe,
        default_hint: "1",
        profiles: &[pv!(Security, "1", "Confirms/restores the safe default.")],
    },
    Tunable {
        key: "fs.suid_dumpable",
        kind: Sysctl,
        category: Category::Security,
        title: "Core dumps from setuid processes",
        description: "Whether a setuid/setgid process is allowed to produce a core dump on crash.",
        why: "A core dump from a privileged process can contain sensitive memory contents (keys, credentials) and be world-readable depending on dump path permissions — disabling this removes that leak.",
        risk: Safe,
        default_hint: "0",
        profiles: &[pv!(Security, "0", "Confirms/restores the safe default.")],
    },

    // ── Kernel modules & memory management (THP) ────────────────────────
    Tunable {
        key: "thp.enabled",
        kind: Sysfs("/sys/kernel/mm/transparent_hugepage/enabled"),
        category: Category::Memory,
        title: "Transparent Huge Pages",
        description: "Whether the kernel automatically backs process memory with 2MB huge pages instead of standard 4KB pages, transparently to the application.",
        why: "Reduces TLB-miss overhead for large, mostly-sequential memory access patterns, but its background defrag/compaction work causes exactly the kind of unpredictable latency spikes that databases with random-access memory patterns (MySQL, PostgreSQL, MongoDB, Redis) have long documented problems with — `madvise` mode still lets an application opt in explicitly (e.g. via hugetlbfs-aware allocators) without the kernel forcing it on everything.",
        risk: Caution,
        default_hint: "always",
        profiles: &[pv!(Database, "madvise", "Long-standing, widely-documented recommendation across MySQL/PostgreSQL/MongoDB/Redis to avoid THP-induced latency spikes."), pv!(AiCompute, "madvise", "Lets allocators that specifically want huge pages request them, without forcing THP everywhere.")],
    },
    Tunable {
        key: "thp.defrag",
        kind: Sysfs("/sys/kernel/mm/transparent_hugepage/defrag"),
        category: Category::Memory,
        title: "THP defrag behavior",
        description: "How aggressively the kernel compacts memory in the background to create huge pages when THP is enabled.",
        why: "Even with `thp.enabled=madvise`, aggressive background defrag/compaction work is itself a source of latency jitter. Setting this to `madvise` too means compaction only happens for allocations that explicitly asked for huge pages, not speculatively in the background.",
        risk: Caution,
        default_hint: "madvise (distro-dependent)",
        profiles: &[pv!(Database, "madvise", "Avoids background compaction jitter for a workload that isn't asking for huge pages anyway.")],
    },
];
