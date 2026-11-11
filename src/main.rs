//! ultra-ram — PSI-driven proactive memory reclaim, leak detection and RAM health monitor.
//!
//! Core idea (same as Meta's TMO / senpai): never drop caches or kill anything.
//! Instead, ask the kernel to move the *coldest* pages of idle apps into compressed
//! swap (zram) through cgroup v2 `memory.reclaim`, in small steps, and watch each
//! app's own memory pressure (PSI). The moment an app starts refaulting, back off.
//! Nothing is lost: a reclaimed page comes back transparently on first access.
//!
//! Runs entirely as the normal user (the systemd user cgroup subtree is delegated).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};

mod age;
mod prefetch;
mod server;
mod zram_tier;

const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
const HISTORY_WINDOW: u64 = 3 * 3600;

// ───────────────────────────── config ─────────────────────────────

/// Desktop: rootless user service, apps = children of the user's `app.slice`.
/// Server: root system service, apps = workload cgroups below the configured roots (server.rs).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum Profile {
    #[default]
    Desktop,
    Server,
}

impl Profile {
    fn parse(s: &str) -> Option<Profile> {
        match s.trim().to_lowercase().as_str() {
            "desktop" => Some(Profile::Desktop),
            "server" => Some(Profile::Server),
            _ => None,
        }
    }
}

/// The `profile = ...` line of a config file, if any.
fn declared_profile(text: &str) -> Option<Profile> {
    text.lines().find_map(|l| {
        let (k, v) = l.split('#').next()?.split_once('=')?;
        (k.trim() == "profile").then(|| Profile::parse(v)).flatten()
    })
}

/// Which profile a command runs: `--profile` wins; otherwise root with `/etc/ultra-ram/config`
/// declaring `profile = server` (what `install.sh --server` writes) gets the server profile, and
/// everyone else the desktop one. A user's own config can never switch to the server profile.
fn resolve_profile(cli: Option<Profile>, root: bool, etc_cfg: &str) -> Profile {
    cli.unwrap_or(if root && declared_profile(etc_cfg) == Some(Profile::Server) { Profile::Server } else { Profile::Desktop })
}

/// Removes `--profile X` / `--profile=X` from the arguments.
fn split_profile(args: &[String]) -> Result<(Vec<String>, Option<Profile>), String> {
    let (mut out, mut prof) = (Vec::new(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let v = if a == "--profile" {
            Some(it.next().ok_or("--profile needs a value: desktop | server")?.clone())
        } else {
            a.strip_prefix("--profile=").map(str::to_string)
        };
        match v {
            Some(v) => prof = Some(Profile::parse(&v).ok_or(format!("unknown profile '{v}' (desktop | server)"))?),
            None => out.push(a.clone()),
        }
    }
    Ok((out, prof))
}

fn is_root() -> bool {
    fs::metadata("/proc/self").map(|m| m.uid() == 0).unwrap_or(false)
}

#[derive(Clone)]
struct Config {
    profile: Profile,
    tick_secs: u64,
    activate_avail_pct: f64,
    min_app_mb: u64,
    step_pct: f64,
    max_step_mb: u64,
    max_offload_pct: f64,
    psi_threshold_pct: f64,
    /// Reclaim counts as wasted (back off, learn the app's ceiling) once this share (%) of what
    /// was recently taken has been faulted back in.
    refault_tolerance_pct: f64,
    active_cpu_pct: f64,
    max_global_psi: f64,
    max_busy_cpu_pct: f64,
    min_swap_free_pct: f64,
    max_zram_ram_pct: f64,
    leak_min_minutes: f64,
    leak_min_slope_mb_h: f64,
    leak_min_growth_mb: f64,
    notify: bool,
    protect: Vec<String>,
    /// With the root age helper running: only memory idle at least this long counts as cold.
    cold_age_secs: u64,
    /// Server profile only: discovery roots (relative to /sys/fs/cgroup) and depth.
    roots: Vec<String>,
    discover_depth: usize,
    /// Server profile only: a workload with memory.min or memory.low set is never reclaimed from.
    respect_memory_low: bool,
}

/// A gate or per-app check set to `off` in the config (or disabled by the server defaults).
const OFF_PCT: f64 = 100.0;

impl Config {
    fn for_profile(p: Profile) -> Config {
        let mut c = Config::default();
        if p == Profile::Server {
            c.profile = p;
            // Servers are always busy and want headroom for packing: no "enough free RAM" idle
            // threshold, no global busy-CPU gate, and a busy workload is not an idle one to skip:
            // its own memory PSI (psi_threshold_pct) decides instead.
            c.activate_avail_pct = OFF_PCT;
            c.max_busy_cpu_pct = OFF_PCT;
            c.active_cpu_pct = f64::INFINITY;
            c.notify = false;
            // A service's rarely read data (a cache's long tail, a pool's spare slots) refaults
            // now and then whatever picks the pages; from swap that costs a fault, not a stall.
            // Its own PSI is the real cost signal, so refaults alone stop reclaim much later.
            c.refault_tolerance_pct = 50.0;
            // Services read parts of their data every few minutes (reports, rare indexes, a
            // cache's tail); 120 s of idleness does not make that cold. The server age helper
            // runs every 150 s (install.sh --server), so the oldest generation is ≥ 300 s idle.
            c.cold_age_secs = 300;
            c.psi_threshold_pct = 1.5;
            c.protect = server::DEFAULT_PROTECT.iter().map(|s| s.to_string()).collect();
        }
        c
    }
    fn discovery(&self) -> server::Discovery {
        server::Discovery { roots: self.roots.clone(), depth: self.discover_depth }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            profile: Profile::Desktop,
            tick_secs: 15,
            activate_avail_pct: 40.0,
            min_app_mb: 100,
            step_pct: 4.0,
            max_step_mb: 64,
            max_offload_pct: 50.0,
            psi_threshold_pct: 0.5,
            refault_tolerance_pct: 12.5,
            active_cpu_pct: 25.0,
            max_global_psi: 10.0,
            max_busy_cpu_pct: 60.0,
            min_swap_free_pct: 25.0,
            max_zram_ram_pct: 25.0,
            leak_min_minutes: 45.0,
            leak_min_slope_mb_h: 60.0,
            leak_min_growth_mb: 150.0,
            notify: true,
            protect: ["gamescope", "wine", "lutris", "heroic"].iter().map(|s| s.to_string()).collect(),
            cold_age_secs: 120,
            roots: server::DEFAULT_ROOTS.iter().map(|s| s.to_string()).collect(),
            discover_depth: server::DEFAULT_DEPTH,
            respect_memory_low: true,
        }
    }
}

const DEFAULT_CONFIG: &str = "\
# ultra-ram ayarları. Dəyişikliklər 1 dəqiqə ərzində avtomatik oxunur.

# Boş (available) RAM bu faizdən çox olanda heç nə edilmir — boş RAM onsuz da kifayətdir.
activate_avail_pct = 40

# Bir addımda tətbiqin RAM-ının ən çox neçə faizi zram-a köçürülə bilər. Addım 1 MB-dan
# başlayır, problem olmadıqca böyüyür (x1.5), təzyiq və ya geri qayıdış görünəndə yarıya düşür.
step_pct = 4
max_step_mb = 64
tick_secs = 15

# Bir tətbiqin yaddaşının ən çox neçə faizi zram-da ola bilər. Bu hədd tətbiqə
# qayıdanda ləngimənin qarşısını alır.
max_offload_pct = 50

# Bundan kiçik tətbiqlərə toxunulmur (MB).
min_app_mb = 100

# Tətbiq yaddaş gözləməsinə bu faizdən çox vaxt itirirsə, ondan əl çəkilir (geri çəkilmə).
psi_threshold_pct = 0.5

# Son vaxtlar köçürülənin bu faizi geri istənilibsə (refault), köçürmə boşa gedib: geri çəkilmə.
refault_tolerance_pct = 12.5

# Tətbiq bir nüvənin bu faizindən çox CPU işlədirsə, aktiv sayılır və toxunulmur.
active_cpu_pct = 25

# Qlobal təhlükəsizlik hədləri.
max_global_psi = 10
# Bütün CPU-ların bu faizindən çoxu məşğuldursa (tətbiq açılır, kompilyasiya, oyun), köçürmə gözləyir.
max_busy_cpu_pct = 60
min_swap_free_pct = 25
max_zram_ram_pct = 25

# Sızma (leak) şübhəsi: ən azı bu qədər dəqiqə dayanmadan, bu sürətlə və bu qədər artım.
leak_min_minutes = 45
leak_min_slope_mb_h = 60
leak_min_growth_mb = 150
notify = true

# Heç vaxt toxunulmayan tətbiqlər (adın bir hissəsi, vergüllə).
protect = gamescope, wine, lutris, heroic

# Yalnız istəyə bağlı root köməkçisi (ultra-ram age run) işləyəndə: bir addımda tətbiqdən ən çox
# bu qədər saniyə toxunulmamış yaddaş qədər köçürülür; belə yaddaş 1 MB-dan azdırsa, tətbiqə
# toxunulmur. Köməkçinin intervalının 2 qatından böyük olmamalıdır (standart interval 60 s).
# Köməkçi yoxdursa bu ayar heç nəyi dəyişmir.
cold_age_secs = 120
";

const DEFAULT_SERVER_CONFIG: &str = "\
# Ultra RAM, server profile (root system service ultra-ram-server.service).
# Changes are picked up within a minute, no restart needed.
profile = server

# Where workloads live, relative to /sys/fs/cgroup (comma-separated; `*` allowed in the last
# component). One workload = one service, container scope, VM or Kubernetes pod. Missing roots
# are skipped; user.slice is never touched (that is the desktop profile's job).
roots = system.slice, machine.slice, kubepods.slice, kubepods, docker, system.slice/docker-*.scope, containerd
# How many grouping levels (slices, QoS classes) to descend below a root.
discover_depth = 4

# Never reclaim from workloads whose name or any of whose processes contains one of these.
protect = postgres, mysqld, mariadbd, redis, memcached, etcd, kubelet, containerd, dockerd, sshd, systemd-journald
# Never reclaim from workloads that have memory.min or memory.low set (operator intent).
respect_memory_low = true

# Per-workload memory PSI target: a workload stalling on memory more than this share of the
# time (%) is backed off exponentially, and the step shrinks as it approaches the target.
# A service reading its rare data back from swap stalls a little all the time; 1.5% of a
# 15 s tick is 225 ms of stall spread over thousands of requests.
psi_threshold_pct = 1.5

# Reclaim counts as wasted once this share (%) of what was recently taken from a workload has
# been faulted back in: it is backed off and its ceiling is learned there. Services read their
# long tail now and then, so this is far looser than on the desktop (12.5); their PSI decides.
refault_tolerance_pct = 50

# Per workload and tick: at most step_pct of it, at most max_step_mb. The step starts at 1 MB,
# grows x1.5 while reclaim is free and halves on a stall or refault.
step_pct = 4
max_step_mb = 64
tick_secs = 15

# At most this share of a workload may live in swap (a learned, lower ceiling applies once the
# workload refaults after a deep offload).
max_offload_pct = 50
# Workloads smaller than this (MB) are left alone.
min_app_mb = 100

# Global safety gates.
max_global_psi = 10
min_swap_free_pct = 25
max_zram_ram_pct = 25

# Desktop gates, off on servers by default (a number turns them back on):
#   activate_avail_pct  only act while available RAM is below this %
#   max_busy_cpu_pct    wait while all CPUs together are busier than this %
#   active_cpu_pct      skip a workload using more than this % of one core
activate_avail_pct = off
max_busy_cpu_pct = off
active_cpu_pct = off

# With the root age helper (ultra-ram age run --profile server): take at most the memory idle
# this long per tick. Keep it <= 2 x the helper's interval (install.sh --server runs it every
# 150 s). Services touch some data every few minutes, so this is longer than on the desktop.
cold_age_secs = 300

# Leak suspects are logged to the journal; notify-send is a desktop thing.
leak_min_minutes = 45
leak_min_slope_mb_h = 60
leak_min_growth_mb = 150
notify = false
";

fn load_config(p: &Paths) -> Config {
    parse_config(&read(&p.cfg.join("config")), p.profile)
}

/// Config text → Config for `profile`: the profile's defaults, then every key that parses, each
/// clamped to a safe range. The `profile` key itself is not read here (see `resolve_profile`).
fn parse_config(text: &str, profile: Profile) -> Config {
    let mut c = Config::for_profile(profile);
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let f = v.parse::<f64>().ok();
        let off = v.eq_ignore_ascii_case("off");
        match (k, f) {
            ("tick_secs", Some(x)) => c.tick_secs = (x as u64).clamp(5, 300),
            ("activate_avail_pct", _) if off => c.activate_avail_pct = OFF_PCT,
            ("max_busy_cpu_pct", _) if off => c.max_busy_cpu_pct = OFF_PCT,
            ("active_cpu_pct", _) if off => c.active_cpu_pct = f64::INFINITY,
            ("activate_avail_pct", Some(x)) => c.activate_avail_pct = x.clamp(5.0, 100.0),
            ("min_app_mb", Some(x)) => c.min_app_mb = (x as u64).max(16),
            ("step_pct", Some(x)) => c.step_pct = x.clamp(0.05, 5.0),
            ("max_step_mb", Some(x)) => c.max_step_mb = (x as u64).clamp(4, 1024),
            ("max_offload_pct", Some(x)) => c.max_offload_pct = x.clamp(0.0, 90.0),
            ("psi_threshold_pct", Some(x)) => c.psi_threshold_pct = x.clamp(0.01, 10.0),
            ("refault_tolerance_pct", Some(x)) => c.refault_tolerance_pct = x.clamp(1.0, 100.0),
            ("active_cpu_pct", Some(x)) => c.active_cpu_pct = x.max(1.0),
            ("max_global_psi", Some(x)) => c.max_global_psi = x.max(0.5),
            ("max_busy_cpu_pct", Some(x)) => c.max_busy_cpu_pct = x.clamp(10.0, 100.0),
            ("discover_depth", Some(x)) => c.discover_depth = (x as usize).clamp(1, server::MAX_DEPTH),
            ("respect_memory_low", _) => c.respect_memory_low = v == "true" || v == "1" || v == "yes",
            ("roots", _) => {
                let r: Vec<String> = v.split(',').filter_map(server::clean_root).collect();
                if !r.is_empty() {
                    c.roots = r;
                }
            }
            ("min_swap_free_pct", Some(x)) => c.min_swap_free_pct = x.clamp(5.0, 95.0),
            ("max_zram_ram_pct", Some(x)) => c.max_zram_ram_pct = x.clamp(5.0, 60.0),
            ("leak_min_minutes", Some(x)) => c.leak_min_minutes = x.max(10.0),
            ("leak_min_slope_mb_h", Some(x)) => c.leak_min_slope_mb_h = x.max(1.0),
            ("leak_min_growth_mb", Some(x)) => c.leak_min_growth_mb = x.max(1.0),
            ("cold_age_secs", Some(x)) => c.cold_age_secs = (x as u64).clamp(30, 3600),
            ("notify", _) => c.notify = v == "true" || v == "1" || v == "yes",
            ("protect", _) => {
                c.protect = v.split(',').map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect()
            }
            _ => {}
        }
    }
    c
}

// ───────────────────────────── paths & helpers ─────────────────────────────

struct Paths {
    profile: Profile,
    user_root: PathBuf,
    app_slice: PathBuf,
    run: PathBuf,
    cfg: PathBuf,
}

impl Paths {
    fn for_profile(p: Profile) -> Paths {
        match p {
            Profile::Desktop => Paths::new(),
            Profile::Server => Paths {
                profile: p,
                user_root: PathBuf::from(server::CGROUP_ROOT),
                app_slice: PathBuf::from(server::CGROUP_ROOT),
                run: PathBuf::from(server::RUN_DIR),
                cfg: PathBuf::from(server::CFG_DIR),
            },
        }
    }
    fn new() -> Paths {
        let uid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(1000);
        let user_root = PathBuf::from(format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service"));
        let run = env::var("XDG_RUNTIME_DIR").unwrap_or(format!("/run/user/{uid}"));
        let cfg = env::var("XDG_CONFIG_HOME")
            .unwrap_or_else(|_| format!("{}/.config", env::var("HOME").unwrap_or_default()));
        Paths {
            profile: Profile::Desktop,
            app_slice: user_root.join("app.slice"),
            user_root,
            run: PathBuf::from(run).join("ultra-ram"),
            cfg: PathBuf::from(cfg).join("ultra-ram"),
        }
    }
}

fn read(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_default()
}
fn read_u64(p: &Path) -> u64 {
    read(p).trim().parse().unwrap_or(0)
}
/// Value of a `key value` line (memory.stat, cpu.stat).
fn kv(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}
/// Value of a `key=value` line (our own status file).
fn kv_eq(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn h(b: u64) -> String {
    if b >= GB {
        format!("{:.1}G", b as f64 / GB as f64)
    } else {
        format!("{}M", b / MB)
    }
}
fn pct(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 * 100.0 / b as f64
    }
}

#[derive(Default, Clone, Copy)]
struct Psi {
    some10: f64,
    some60: f64,
    some300: f64,
    full10: f64,
    some_total: u64,
}

fn psi(p: &Path) -> Psi {
    let mut r = Psi::default();
    for line in read(p).lines() {
        let mut it = line.split_whitespace();
        let kind = it.next().unwrap_or("");
        for tok in it {
            let Some((k, v)) = tok.split_once('=') else { continue };
            match (kind, k) {
                ("some", "avg10") => r.some10 = v.parse().unwrap_or(0.0),
                ("some", "avg60") => r.some60 = v.parse().unwrap_or(0.0),
                ("some", "avg300") => r.some300 = v.parse().unwrap_or(0.0),
                ("some", "total") => r.some_total = v.parse().unwrap_or(0),
                ("full", "avg10") => r.full10 = v.parse().unwrap_or(0.0),
                _ => {}
            }
        }
    }
    r
}

#[derive(Default, Clone, Copy)]
struct Mem {
    total: u64,
    avail: u64,
    free: u64,
    cached: u64,
    swap_total: u64,
    swap_free: u64,
    kernel: u64,
}

fn meminfo() -> Mem {
    let t = read(Path::new("/proc/meminfo"));
    let g = |k: &str| -> u64 {
        t.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(':')))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    Mem {
        total: g("MemTotal"),
        avail: g("MemAvailable"),
        free: g("MemFree"),
        cached: g("Cached") + g("Buffers"),
        swap_total: g("SwapTotal"),
        swap_free: g("SwapFree"),
        kernel: g("Slab") + g("PageTables") + g("KernelStack"),
    }
}

/// (original bytes stored, compressed bytes, real RAM used) summed over all zram devices.
fn zram() -> (u64, u64, u64) {
    let mut r = (0, 0, 0);
    if let Ok(rd) = fs::read_dir("/sys/block") {
        for e in rd.flatten() {
            if !e.file_name().to_string_lossy().starts_with("zram") {
                continue;
            }
            let s = read(&e.path().join("mm_stat"));
            let v: Vec<u64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 3 {
                r = (r.0 + v[0], r.1 + v[1], r.2 + v[2]);
            }
        }
    }
    r
}

// ───────────────────────────── cgroup sampling ─────────────────────────────

struct Cg {
    path: PathBuf,
    current: u64,
    swap: u64,
    /// Leak metric: anonymous + shmem + swapped memory (page cache excluded).
    footprint: u64,
    psi_total: u64,
    cpu_usec: u64,
    /// Anonymous pages faulted back in after reclaim (`workingset_refault_anon`), cumulative.
    refault_anon: u64,
    /// memory.pressure `some avg10` (status output only).
    psi10: f64,
    /// Never reclaim from it (server profile: protect list, memory.min/low). The desktop profile
    /// decides protection by app name in `pass()` and leaves this false.
    protected: bool,
}

type Apps = BTreeMap<String, Vec<Cg>>;

fn sample_cg(path: PathBuf) -> Cg {
    let current = read_u64(&path.join("memory.current"));
    let swap = read_u64(&path.join("memory.swap.current"));
    let stat = read(&path.join("memory.stat"));
    let cache = kv(&stat, "file").saturating_sub(kv(&stat, "shmem"));
    let pr = psi(&path.join("memory.pressure"));
    Cg {
        footprint: current.saturating_sub(cache) + swap,
        psi_total: pr.some_total,
        psi10: pr.some10,
        protected: false,
        cpu_usec: kv(&read(&path.join("cpu.stat")), "usage_usec"),
        refault_anon: kv(&stat, "workingset_refault_anon"),
        current,
        swap,
        path,
    }
}

/// `app-com.google.Chrome@<hash>.service` / `app-flatpak-com.google.Chrome-123.scope` → `com.google.Chrome`
fn app_key(dir: &str) -> String {
    let mut s = dir.replace("\\x2d", "-");
    for suf in [".service", ".scope", ".slice"] {
        if let Some(x) = s.strip_suffix(suf) {
            s = x.to_string();
            break;
        }
    }
    if let Some(x) = s.strip_prefix("app-") {
        s = x.to_string();
    }
    if let Some(i) = s.find('@') {
        s.truncate(i);
    }
    if let Some(x) = s.strip_prefix("dbus-:") {
        if let Some(i) = x.find('-') {
            s = x[i + 1..].to_string();
        }
    }
    if let Some(x) = s.strip_prefix("flatpak-") {
        s = x.to_string();
    }
    if let Some(i) = s.rfind('-') {
        if i + 1 < s.len() && s[i + 1..].bytes().all(|b| b.is_ascii_digit()) {
            s.truncate(i);
        }
    }
    s
}

/// Short human name: `com.google.Chrome` → `Chrome`, `org.telegram.desktop` → `telegram.desktop`.
fn disp(key: &str) -> String {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.len() < 3 {
        return key.to_string();
    }
    let last = parts[parts.len() - 1];
    if ["browser", "desktop", "client", "app"].contains(&last.to_lowercase().as_str()) {
        format!("{}.{}", parts[parts.len() - 2], last)
    } else {
        last.to_string()
    }
}

fn scan(slice: &Path) -> Apps {
    let mut apps = Apps::new();
    if let Ok(rd) = fs::read_dir(slice) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let key = app_key(&e.file_name().to_string_lossy());
                apps.entry(key).or_default().push(sample_cg(e.path()));
            }
        }
    }
    apps
}

/// Protection verdicts of server workloads, refreshed every minute (reading every process's comm
/// on every tick would be wasteful).
type ProtCache = HashMap<PathBuf, Option<String>>;

/// Server profile: one app per workload cgroup, keyed by its path relative to the cgroup root.
fn scan_server(cfg: &Config, cgroot: &Path, self_cg: &Path, prot: &mut ProtCache) -> Apps {
    let mut apps = Apps::new();
    for path in server::workloads(cgroot, &cfg.discovery(), Some(self_cg)) {
        let reason = prot
            .entry(path.clone())
            .or_insert_with(|| {
                let name = server::workload_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
                server::protection(&path, &name, &cfg.protect, cfg.respect_memory_low, &server::proc_comm)
            })
            .is_some();
        let mut cg = sample_cg(path);
        cg.protected = reason;
        apps.entry(server::rel_key(cgroot, &cg.path)).or_default().push(cg);
    }
    apps
}

fn self_cgroup() -> PathBuf {
    let s = read(Path::new("/proc/self/cgroup"));
    let rel = s.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("/").trim().to_string();
    PathBuf::from(format!("/sys/fs/cgroup{rel}"))
}

fn is_protected(cfg: &Config, key: &str) -> bool {
    let k = key.to_lowercase();
    cfg.protect.iter().any(|p| k.contains(p))
}

/// Sub-cgroup levels `pids_in` descends at most. Users can nest their own cgroup trees
/// arbitrarily deep, and prefetch walks them as root.
const PIDS_MAX_DEPTH: usize = 32;

fn pids_in(dir: &Path, out: &mut Vec<u32>) {
    pids_in_depth(dir, out, 0);
}

fn pids_in_depth(dir: &Path, out: &mut Vec<u32>, depth: usize) {
    out.extend(read(&dir.join("cgroup.procs")).lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    if depth >= PIDS_MAX_DEPTH {
        return;
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            // DirEntry::file_type does not follow symlinks.
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                pids_in_depth(&e.path(), out, depth + 1);
            }
        }
    }
}

/// (name, rss bytes, swap bytes) of a process.
fn proc_mem(pid: u32) -> Option<(String, u64, u64)> {
    let s = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let g = |k: &str| -> u64 {
        s.lines()
            .find_map(|l| l.strip_prefix(k))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let name = s.lines().next()?.strip_prefix("Name:")?.trim().to_string();
    Some((name, g("VmRSS:"), g("VmSwap:")))
}

fn top_proc(cgs: &[Cg]) -> Option<(u32, String, u64)> {
    let mut pids = Vec::new();
    for c in cgs {
        pids_in(&c.path, &mut pids);
    }
    pids.into_iter()
        .filter_map(|p| proc_mem(p).map(|(n, r, s)| (p, n, r + s)))
        .max_by_key(|x| x.2)
}

// ───────────────────────────── reclaim engine ─────────────────────────────

#[derive(Default)]
struct Prev {
    psi: u64,
    cpu: u64,
    level: u32,
    skip_until: u64,
    p: f64,
    c: f64,
    /// Refault counter at the previous tick.
    refault: u64,
    /// Adaptive per-app step in bytes (AIMD): grows while reclaim is free, halves when it hurts.
    step: f64,
    /// Exponentially decayed bytes we reclaimed from this app (time constant REFAULT_TAU).
    recl_w: f64,
    /// Exponentially decayed bytes the app faulted back in over the same window.
    ref_w: f64,
    /// Tick of the last reclaim from this app.
    last_rc: u64,
    /// Learned ceiling (% of the app in swap). Set when the app wanted memory back after a deep
    /// offload: that is where its cold part ends, so we stop just short of it from then on.
    learned_cap: f64,
    /// Bytes faulted back in since we started watching this cgroup (status output only).
    refault_total: u64,
}

/// First step for an app we have not reclaimed from yet. Small on purpose: right after an app
/// loads, the kernel's LRU cannot yet tell its hot pages from cold ones.
const PROBE_STEP: f64 = MB as f64;
const STEP_GROWTH: f64 = 1.5;
/// How far below the point where an app first refaulted we put its learned ceiling.
const LEARNED_MARGIN_PCT: f64 = 2.0;

/// The root age helper keeps its output directory while it runs (systemd RuntimeDirectory).
fn age_helper_running() -> bool {
    let fresh = |p: &Path| {
        fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_secs() < 600)
    };
    // The directory itself counts too: it is created at start-up, before the first map exists.
    let dir = Path::new(age::AGE_DIR);
    fresh(dir) || fs::read_dir(dir).map(|rd| rd.flatten().any(|e| fresh(&e.path()))).unwrap_or(false)
}
/// Window over which refaults are matched against reclaim. A page taken now may be wanted back
/// many ticks later (a tab looked at once a minute), so comparing tick-to-tick misses it.
const REFAULT_TAU: f64 = 120.0;
/// At most this share of an app may be reclaimed per REFAULT_TAU window, so an app that is
/// merely quiet (no stall, no refault yet) is not emptied before it can object.
const MAX_WINDOW_SHARE: f64 = 0.10;
/// Server profile: 3.5x that. Its age map already limits reclaim to memory idle ≥ 300 s, which
/// has had time to object, and the longer cold age leaves less of the window to act in.
const MAX_WINDOW_SHARE_SERVER: f64 = 0.35;
/// After this many ticks without reclaiming from an app, what we knew about it is stale: probe again.
const STALE_TICKS: u64 = 8;
/// First pause after a refault back-off, in ticks (5 min at the default tick); doubles each time.
const REFAULT_PAUSE_TICKS: u64 = 20;
/// Share of an app already in zram above which a refault is taken as "the cold part is used up".
const DEEP_OFFLOAD_PCT: f64 = 15.0;

/// Kernel page size (4K on x86, 16K/64K on some arm64/ppc64 kernels); refault counters are in pages.
fn page_size() -> u64 {
    static PS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *PS.get_or_init(|| {
        read(Path::new("/proc/self/smaps"))
            .lines()
            .find_map(|l| l.strip_prefix("KernelPageSize:"))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map(|k| k * 1024)
            .filter(|b| b.is_power_of_two())
            .unwrap_or(4096)
    })
}

#[derive(Default)]
struct Tracker {
    prev: HashMap<PathBuf, Prev>,
    tick: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Observe,
    Dry,
    Real,
}

/// Which `memory.reclaim` syntax this kernel accepts: 0 = `swappiness=max` (anon only),
/// 1 = `swappiness=200`, 2 = plain byte count.
static RECLAIM_FMT: AtomicU8 = AtomicU8::new(0);

/// Ask the kernel to reclaim `bytes` of cold memory from one cgroup.
///
/// Only anonymous memory is targeted: page cache is already counted as available, so
/// dropping it frees nothing and only costs disk re-reads. Cold anonymous pages go to
/// zram, where they shrink ~3x — that is the real gain.
fn reclaim(cg: &Path, bytes: u64) {
    loop {
        let f = RECLAIM_FMT.load(Ordering::Relaxed);
        let arg = match f {
            0 => format!("{bytes} swappiness=max"),
            1 => format!("{bytes} swappiness=200"),
            _ => bytes.to_string(),
        };
        match fs::write(cg.join("memory.reclaim"), arg) {
            // EINVAL = this kernel does not know the option; try the next syntax.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput && f < 2 => {
                RECLAIM_FMT.store(f + 1, Ordering::Relaxed)
            }
            // EAGAIN just means "could not reclaim the full amount" — the caller measures.
            _ => return,
        }
    }
}

/// One pass over all app cgroups. Returns bytes reclaimed (or planned, in Dry mode) per app.
fn pass(cfg: &Config, apps: &Apps, tr: &mut Tracker, dt: f64, mode: Mode, self_cg: &Path) -> BTreeMap<String, u64> {
    tr.tick += 1;
    let tick = tr.tick;
    let t_now = now();
    let mut out = BTreeMap::new();
    let window_us = dt.max(0.5) * 1e6;
    for (key, cgs) in apps {
        let prot = is_protected(cfg, key);
        for cg in cgs {
            // Counters went backwards: the cgroup was recreated at the same path (app restarted).
            // Forget what we knew, or the fresh app inherits the old one's grown step.
            if tr.prev.get(&cg.path).is_some_and(|e| cg.psi_total < e.psi || cg.cpu_usec < e.cpu) {
                tr.prev.remove(&cg.path);
            }
            let first = !tr.prev.contains_key(&cg.path);
            let e = tr.prev.entry(cg.path.clone()).or_default();
            if first {
                e.psi = cg.psi_total;
                e.cpu = cg.cpu_usec;
                e.refault = cg.refault_anon;
                continue; // need a baseline before judging
            }
            let p = cg.psi_total.saturating_sub(e.psi) as f64 / window_us * 100.0;
            let c = cg.cpu_usec.saturating_sub(e.cpu) as f64 / window_us * 100.0;
            // Pages we pushed out earlier that the app wanted back since the last tick.
            let refaulted = cg.refault_anon.saturating_sub(e.refault).saturating_mul(page_size());
            e.psi = cg.psi_total;
            e.cpu = cg.cpu_usec;
            e.refault = cg.refault_anon;
            e.p = p;
            e.c = c;
            e.refault_total = e.refault_total.saturating_add(refaulted);
            if e.step <= 0.0 {
                e.step = PROBE_STEP;
            }
            // Updated on every tick, including skipped ones, so late refaults are not lost.
            let decay = (-dt.max(0.5) / REFAULT_TAU).exp();
            e.recl_w *= decay;
            e.ref_w = e.ref_w * decay + refaulted as f64;
            let swap_pct = pct(cg.swap, cg.current + cg.swap);
            // Wasted work: refault_tolerance_pct of what we recently took came back (an eighth on
            // the desktop). Those pages were not cold.
            let wasted = e.recl_w >= PROBE_STEP / 2.0
                && e.ref_w >= PROBE_STEP / 2.0
                && e.ref_w * 100.0 >= e.recl_w * cfg.refault_tolerance_pct;
            // A refault after a deep offload marks where this app's cold memory ends. Learn it.
            // On the desktop any refault counts, even one too small to be wasted work: the next
            // bite would be hot pages. A server workload refaults a little all the time, so only
            // wasted work moves its ceiling.
            let strict = cfg.profile == Profile::Desktop;
            if (if strict { refaulted > 0 } else { wasted }) && swap_pct >= DEEP_OFFLOAD_PCT {
                let edge = (swap_pct - LEARNED_MARGIN_PCT).max(0.0);
                e.learned_cap = if e.learned_cap > 0.0 { e.learned_cap.min(edge) } else { edge };
            }
            if wasted {
                e.ref_w = 0.0; // one back-off per burst
            }
            // The app is stalling on memory, or refaulting what we took: back off exponentially
            // and halve the step (multiplicative decrease).
            if p >= cfg.psi_threshold_pct || wasted {
                e.level = (e.level + 1).min(5);
                // A stall can be a passing spike. A refault once a good part of the app is already
                // offloaded means we have reached memory it really uses, and that does not change
                // in a few seconds: stay away much longer. Early refaults are different — before
                // the first reclaims the kernel has no page ages, so the first bites are random.
                let deep = pct(cg.swap, cg.current + cg.swap) >= DEEP_OFFLOAD_PCT;
                let pause = if wasted && deep { REFAULT_PAUSE_TICKS << (e.level - 1) } else { 1u64 << e.level };
                e.skip_until = tick + pause;
                e.step = (e.step / 2.0).max(PROBE_STEP);
                continue;
            }
            if mode == Mode::Observe || prot || cg.protected || cg.current < cfg.min_app_mb * MB || self_cg.starts_with(&cg.path) {
                continue;
            }
            if tick < e.skip_until || c > cfg.active_cpu_pct {
                continue;
            }
            // Hard bound on how much of one app may live in zram, so switching back to it stays snappy.
            if swap_pct >= cfg.max_offload_pct || (e.learned_cap > 0.0 && swap_pct >= e.learned_cap) {
                continue;
            }
            // "Calm": on the desktop, no stall at all. A server workload that reads swap now and
            // then always shows a trace of PSI; well under its target counts as calm too, or the
            // step never grows past its first megabytes.
            let calm = if cfg.profile == Profile::Server { p < cfg.psi_threshold_pct / 4.0 } else { p == 0.0 };
            if calm && e.level > 0 {
                e.level -= 1;
            }
            let share = if cfg.profile == Profile::Server { MAX_WINDOW_SHARE_SERVER } else { MAX_WINDOW_SHARE };
            if e.recl_w > cg.current as f64 * share {
                continue;
            }
            if tick.saturating_sub(e.last_rc) > STALE_TICKS {
                e.step = PROBE_STEP;
            }
            // Root age helper (`ultra-ram age run`), when its map of this app is fresh: never ask
            // for more than the memory it saw idle for cold_age_secs, and leave the app alone when
            // that is under 1 MB. Without the helper there is no file and nothing changes.
            let age_file = age::file_for(&cg.path);
            let cold = age_file.as_ref().and_then(|f| age::cold_bytes(&read(f), t_now, cfg.cold_age_secs));
            if cold.is_some_and(|b| b < MB) {
                continue;
            }
            // The helper is running but has no fresh map of this app yet (just started, or a new
            // app): wait for it rather than reclaim blind.
            if cold.is_none() && age_helper_running() {
                continue;
            }
            // Never take more than step_pct of the app per tick, and never more than max_step_mb.
            let cap = (cg.current as f64 * cfg.step_pct / 100.0).clamp(PROBE_STEP, (cfg.max_step_mb * MB) as f64);
            let want = (e.step.min(cap) * (1.0 - p / cfg.psi_threshold_pct)).max(PROBE_STEP) as u64;
            let want = cold.map_or(want, |b| want.min(b));
            let got = if mode == Mode::Dry {
                want
            } else {
                // Count what really moved into zram, not what was asked for.
                reclaim(&cg.path, want);
                read_u64(&cg.path.join("memory.swap.current")).saturating_sub(cg.swap)
            };
            e.last_rc = tick;
            if mode == Mode::Real {
                e.recl_w += got as f64;
            }
            // Additive-ish increase: reclaim was free (calm, no refault) → take a bigger bite next time.
            if calm && got * 2 >= want {
                e.step = (e.step * STEP_GROWTH).min((cfg.max_step_mb * MB) as f64);
            }
            if got > 0 {
                *out.entry(key.clone()).or_insert(0) += got;
            }
        }
    }
    if tick % 200 == 0 {
        tr.prev.retain(|p, _| p.exists());
    }
    out
}

/// Global safety gate. Err((stable code, human message)) means: do not reclaim now.
/// (busy, total) jiffies summed over all CPUs, from the first line of /proc/stat.
fn cpu_times() -> (u64, u64) {
    let s = read(Path::new("/proc/stat"));
    let v: Vec<u64> = s
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    let total: u64 = v.iter().take(8).sum();
    // idle + iowait are not busy.
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    (total.saturating_sub(idle), total)
}

/// Share of all CPUs that was busy between two `cpu_times()` samples, in percent.
fn cpu_busy_pct(a: (u64, u64), b: (u64, u64)) -> f64 {
    let total = b.1.saturating_sub(a.1);
    if total == 0 {
        return 0.0;
    }
    b.0.saturating_sub(a.0) as f64 * 100.0 / total as f64
}

/// Why the global gate is closed. The stable code goes to logs and status; the message is in the
/// profile's language (desktop: Azerbaijani, server: English for operators).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Gate {
    Paused,
    Busy(f64),
    Enough(f64),
    NoSwap,
    SwapFull(f64),
    ZramFull(f64),
    Pressure(f64),
}

impl Gate {
    fn code(&self) -> &'static str {
        match self {
            Gate::Paused => "paused",
            Gate::Busy(_) => "busy",
            Gate::Enough(_) => "enough",
            Gate::NoSwap => "noswap",
            Gate::SwapFull(_) => "swapfull",
            Gate::ZramFull(_) => "zramfull",
            Gate::Pressure(_) => "pressure",
        }
    }
    fn msg(&self, p: Profile) -> String {
        match (p, *self) {
            (Profile::Desktop, Gate::Paused) => "fasilədədir (ultra-ram resume ilə davam etdirin)".to_string(),
            (Profile::Desktop, Gate::Busy(busy)) => format!("sistem məşğuldur (CPU {busy:.0}%) — köçürmə təxirə salındı"),
            (Profile::Desktop, Gate::Enough(avail)) => format!("boş RAM kifayətdir ({avail:.0}%) — müdaxiləyə ehtiyac yoxdur"),
            (Profile::Desktop, Gate::NoSwap) => "swap/zram yoxdur — soyuq yaddaşı köçürməyə yer yoxdur".into(),
            (Profile::Desktop, Gate::SwapFull(sf)) => format!("swap-da boş yer azdır ({sf:.0}%) — dayandırıldı"),
            (Profile::Desktop, Gate::ZramFull(zr)) => format!("zram RAM-ın {zr:.0}%-ni tutur — dayandırıldı"),
            (Profile::Desktop, Gate::Pressure(g)) => {
                format!("sistem yaddaş təzyiqi altındadır (PSI {g:.1}) — əlavə yük yaradılmır")
            }
            (Profile::Server, Gate::Paused) => "paused (ultra-ram resume --profile server)".to_string(),
            (Profile::Server, Gate::Busy(busy)) => format!("system busy (CPU {busy:.0}%) — reclaim deferred"),
            (Profile::Server, Gate::Enough(avail)) => format!("enough free RAM ({avail:.0}%) — nothing to do"),
            (Profile::Server, Gate::NoSwap) => "no swap or zram — nowhere to offload cold memory".into(),
            (Profile::Server, Gate::SwapFull(sf)) => format!("little free swap ({sf:.0}%) — stopped"),
            (Profile::Server, Gate::ZramFull(zr)) => format!("zram holds {zr:.0}% of RAM — stopped"),
            (Profile::Server, Gate::Pressure(g)) => format!("system under memory pressure (PSI {g:.1}) — standing down"),
        }
    }
}

fn active_msg(p: Profile) -> String {
    match p {
        Profile::Desktop => "aktiv — soyuq yaddaş zram-a köçürülür".to_string(),
        Profile::Server => "active — offloading cold memory".to_string(),
    }
}

/// The global safety gate as a pure function of its inputs (`zram_ram`: real RAM held by zram,
/// `psi10`: system memory PSI some avg10).
fn gate_check(cfg: &Config, m: &Mem, check_avail: bool, zram_ram: u64, psi10: f64) -> Result<(), Gate> {
    let avail = pct(m.avail, m.total);
    if check_avail && avail > cfg.activate_avail_pct {
        return Err(Gate::Enough(avail));
    }
    if m.swap_total == 0 {
        return Err(Gate::NoSwap);
    }
    let sf = pct(m.swap_free, m.swap_total);
    if sf < cfg.min_swap_free_pct {
        return Err(Gate::SwapFull(sf));
    }
    let zr = pct(zram_ram, m.total);
    if zr > cfg.max_zram_ram_pct {
        return Err(Gate::ZramFull(zr));
    }
    if psi10 > cfg.max_global_psi {
        return Err(Gate::Pressure(psi10));
    }
    Ok(())
}

/// The daemon's per-tick gate: pause, then the busy-CPU gate, then the global safety gate. With
/// the server defaults the busy-CPU and available-RAM checks are off (`OFF_PCT`: a share can
/// never exceed 100%), so only pause, swap, zram and PSI can stop it.
fn tick_gate(cfg: &Config, m: &Mem, busy: f64, paused: bool, zram_ram: u64, psi10: f64) -> Result<(), Gate> {
    if paused {
        Err(Gate::Paused)
    } else if busy > cfg.max_busy_cpu_pct {
        // Someone is working (an app launching, a build, a game): reclaim costs CPU and
        // memory bandwidth right when they are needed. Cold memory can wait a tick.
        Err(Gate::Busy(busy))
    } else {
        gate_check(cfg, m, true, zram_ram, psi10)
    }
}

fn gate(cfg: &Config, m: &Mem, check_avail: bool) -> Result<(), (&'static str, String)> {
    gate_check(cfg, m, check_avail, zram().2, psi(Path::new("/proc/pressure/memory")).some10)
        .map_err(|g| (g.code(), g.msg(cfg.profile)))
}

// ───────────────────────────── trend / leak detection ─────────────────────────────

struct Trend {
    slope: f64,
    span_min: f64,
    growth: f64,
    suspect: bool,
}

/// Least-squares fit over the recent history. `hist` = (unix ts, footprint MB).
fn trend(hist: &[(u64, f64)], cfg: &Config, t_now: u64) -> Option<Trend> {
    let pts: Vec<(u64, f64)> = hist.iter().copied().filter(|(t, _)| t_now.saturating_sub(*t) <= HISTORY_WINDOW).collect();
    if pts.len() < 10 {
        return None;
    }
    let t0 = pts[0].0;
    let n = pts.len() as f64;
    let xs: Vec<f64> = pts.iter().map(|(t, _)| t.saturating_sub(t0) as f64 / 3600.0).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = pts.iter().map(|(_, y)| *y).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for (x, (_, y)) in xs.iter().zip(pts.iter()) {
        sxx += (x - mx) * (x - mx);
        sxy += (x - mx) * (y - my);
        syy += (y - my) * (y - my);
    }
    if sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let r2 = if syy > 0.0 { sxy * sxy / (sxx * syy) } else { 0.0 };
    let span_h = xs[xs.len() - 1];
    let growth = slope * span_h;
    Some(Trend {
        slope,
        span_min: span_h * 60.0,
        growth,
        suspect: span_h * 60.0 >= cfg.leak_min_minutes
            && slope >= cfg.leak_min_slope_mb_h
            && r2 >= 0.85
            && growth >= cfg.leak_min_growth_mb,
    })
}

type History = HashMap<String, Vec<(u64, f64)>>;

fn load_history(p: &Paths) -> History {
    let mut hmap = History::new();
    let t = now();
    for line in read(&p.run.join("history")).lines() {
        let mut it = line.split('\t');
        let (Some(ts), Some(key), Some(mb)) = (it.next(), it.next(), it.next()) else { continue };
        let (Ok(ts), Ok(mb)) = (ts.parse::<u64>(), mb.parse::<f64>()) else { continue };
        if t.saturating_sub(ts) <= HISTORY_WINDOW {
            hmap.entry(key.to_string()).or_default().push((ts, mb));
        }
    }
    for v in hmap.values_mut() {
        v.sort_by_key(|x| x.0);
    }
    hmap
}

fn save_history(p: &Paths, hist: &History) {
    let mut s = String::new();
    for (k, v) in hist {
        for (ts, mb) in v {
            s.push_str(&format!("{ts}\t{k}\t{mb:.0}\n"));
        }
    }
    write_atomic(&p.run.join("history"), &s);
}

fn write_atomic(path: &Path, content: &str) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, content).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn notify(title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "Ultra RAM", "-i", "dialog-warning", "-u", "normal", title, body])
        .status();
}

// ───────────────────────────── limits (opt-in soft caps) ─────────────────────────────

fn load_limits(p: &Paths) -> BTreeMap<String, u64> {
    read(&p.cfg.join("limits"))
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once('\t')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

fn save_limits(p: &Paths, l: &BTreeMap<String, u64>) {
    let _ = fs::create_dir_all(&p.cfg);
    let s: String = l.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
    write_atomic(&p.cfg.join("limits"), &s);
}

fn enforce_limits(p: &Paths, apps: &Apps) {
    for (key, mb) in load_limits(p) {
        let want = (mb * MB).to_string();
        for cg in apps.get(&key).map(|v| v.as_slice()).unwrap_or(&[]) {
            let f = cg.path.join("memory.high");
            if read(&f).trim() != want {
                let _ = fs::write(&f, &want);
            }
        }
    }
}

fn reset_high(apps: &Apps, key: &str) -> usize {
    let mut n = 0;
    for cg in apps.get(key).map(|v| v.as_slice()).unwrap_or(&[]) {
        if fs::write(cg.path.join("memory.high"), "max").is_ok() {
            n += 1;
        }
    }
    n
}

fn resolve_app(apps: &Apps, name: &str) -> Result<String, String> {
    let n = name.to_lowercase();
    let exact: Vec<&String> = apps.keys().filter(|k| k.to_lowercase() == n || disp(k).to_lowercase() == n).collect();
    let cands: Vec<&String> =
        if exact.is_empty() { apps.keys().filter(|k| k.to_lowercase().contains(&n)).collect() } else { exact };
    match cands.len() {
        1 => Ok(cands[0].clone()),
        0 => Err(format!("'{name}' adlı işləyən tətbiq tapılmadı (siyahı: ultra-ram analyze)")),
        _ => Err(format!(
            "'{name}' bir neçə tətbiqə uyğundur: {}",
            cands.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

// ───────────────────────────── daemon ─────────────────────────────

#[derive(Default)]
struct AppState {
    reclaimed: u64,
    last_notify: u64,
    slope: f64,
    leak: bool,
}

/// Per-workload state for the server status (same order of checks as `pass()`).
fn workload_state(cfg: &Config, cg: &Cg, e: Option<&Prev>, tick: u64, gate_open: bool) -> &'static str {
    let Some(e) = e else { return "new" };
    let swap_pct = pct(cg.swap, cg.current + cg.swap);
    if cg.protected {
        "protected"
    } else if cg.current < cfg.min_app_mb * MB {
        "small"
    } else if e.p >= cfg.psi_threshold_pct || (e.level > 0 && tick < e.skip_until) {
        "backoff"
    } else if e.c > cfg.active_cpu_pct {
        "active"
    } else if swap_pct >= cfg.max_offload_pct || (e.learned_cap > 0.0 && swap_pct >= e.learned_cap) {
        "capped"
    } else if !gate_open {
        "gated"
    } else if e.last_rc == tick && tick > 0 {
        "reclaiming"
    } else {
        "idle"
    }
}

/// Writes /run/ultra-ram/status.json and its text twin status.txt.
#[allow(clippy::too_many_arguments)]
fn write_server_status(
    p: &Paths,
    cfg: &Config,
    apps: &Apps,
    tr: &Tracker,
    st: &HashMap<String, AppState>,
    prot: &ProtCache,
    gate: (&str, &str),
    gate_open: bool,
    mem: &Mem,
    started: u64,
    total: u64,
) {
    let g = psi(Path::new("/proc/pressure/memory"));
    let mut snap = server::Snapshot {
        updated: now(),
        started,
        gate_code: gate.0.to_string(),
        gate_msg: gate.1.to_string(),
        reclaimed_total: total,
        mem_total: mem.total,
        mem_avail: mem.avail,
        swap_total: mem.swap_total,
        swap_free: mem.swap_free,
        zram_used: zram().2,
        psi_some10: g.some10,
        psi_some60: g.some60,
        psi_full10: g.full10,
        system_slice_psi10: psi(&Path::new(server::CGROUP_ROOT).join("system.slice/memory.pressure")).some10,
        roots: cfg.roots.clone(),
        workloads: Vec::new(),
    };
    for (key, cgs) in sorted(apps) {
        for cg in cgs {
            let e = tr.prev.get(&cg.path);
            let dir = cg.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let a = st.get(key);
            snap.workloads.push(server::WorkloadRow {
                cgroup: key.clone(),
                name: server::workload_name(&dir),
                memory: cg.current,
                swap: cg.swap,
                reclaimed: a.map_or(0, |a| a.reclaimed),
                psi10: cg.psi10,
                psi_tick: e.map_or(0.0, |e| e.p),
                refault: e.map_or(0, |e| e.refault_total),
                learned_cap: e.map_or(0.0, |e| e.learned_cap),
                state: workload_state(cfg, cg, e, tr.tick, gate_open),
                protected: prot.get(&cg.path).cloned().flatten(),
                leak: a.is_some_and(|a| a.leak),
            });
        }
    }
    write_atomic(&p.run.join("status.json"), &server::render_json(&snap));
    write_atomic(&p.run.join("status.txt"), &server::render_text(&snap));
}

fn daemon(profile: Profile) {
    let srv = profile == Profile::Server;
    let p = Paths::for_profile(profile);
    let cgroot = PathBuf::from(server::CGROUP_ROOT);
    if srv {
        if !cgroot.join("cgroup.controllers").exists() {
            eprintln!("cgroup v2 not found at {}", cgroot.display());
            std::process::exit(1);
        }
        if !is_root() {
            eprintln!("warning: the server profile runs as root: memory.reclaim of system cgroups is root-owned");
        }
    } else {
        if !p.app_slice.exists() {
            eprintln!("cgroup v2 istifadəçi ağacı tapılmadı: {}", p.app_slice.display());
            std::process::exit(1);
        }
        let own = p.cfg.join("config");
        if declared_profile(&read(&own)) == Some(Profile::Server) {
            println!(
                "note: 'profile = server' in {} is ignored: the server profile runs as root from {}/config (ultra-ram daemon --profile server)",
                own.display(),
                server::CFG_DIR
            );
        }
    }
    let _ = fs::create_dir_all(&p.run);
    let mut cfg = load_config(&p);
    let self_cg = self_cgroup();
    let mut tr = Tracker::default();
    let mut hist = load_history(&p);
    let mut st: HashMap<String, AppState> = HashMap::new();
    let mut prot = ProtCache::new();
    let (mut last, mut last_min, mut last_log) = (Instant::now(), 0u64, now());
    let mut cpu_prev = cpu_times();
    let (mut total, mut since_log, mut gate_code) = (0u64, 0u64, "");
    let mut avail_at_log = meminfo().avail;
    let started = now();
    if srv {
        println!(
            "ultra-ram daemon started (server profile, tick {}s, roots: {}, protect: {})",
            cfg.tick_secs,
            cfg.roots.join(", "),
            cfg.protect.join(", ")
        );
    } else {
        println!(
            "ultra-ram daemon başladı (addım {}s, boş RAM < {}% olanda işə düşür)",
            cfg.tick_secs, cfg.activate_avail_pct
        );
    }

    loop {
        thread::sleep(Duration::from_secs(cfg.tick_secs));
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();
        let apps = if srv { scan_server(&cfg, &cgroot, &self_cg, &mut prot) } else { scan(&p.app_slice) };
        let mem = meminfo();
        let cpu_now = cpu_times();
        let busy = cpu_busy_pct(cpu_prev, cpu_now);
        cpu_prev = cpu_now;
        let paused = p.run.join("paused").exists();
        let g = tick_gate(&cfg, &mem, busy, paused, zram().2, psi(Path::new("/proc/pressure/memory")).some10);
        let (code, msg) = match &g {
            Ok(()) => ("active", active_msg(profile)),
            Err(x) => (x.code(), x.msg(profile)),
        };
        if code != gate_code {
            if srv {
                println!("state: {msg}");
            } else {
                println!("vəziyyət: {msg}");
            }
            gate_code = code;
        }
        // A long gap means suspend/resume: counters are meaningless for this tick, only re-baseline.
        let mode = if g.is_ok() && dt < cfg.tick_secs as f64 * 3.0 { Mode::Real } else { Mode::Observe };
        for (k, b) in pass(&cfg, &apps, &mut tr, dt, mode, &self_cg) {
            total += b;
            since_log += b;
            st.entry(k).or_default().reclaimed += b;
        }

        let t = now();
        if t.saturating_sub(last_min) >= 60 {
            last_min = t;
            cfg = load_config(&p);
            enforce_limits(&p, &apps);
            for (key, cgs) in &apps {
                let fp = cgs.iter().map(|c| c.footprint).sum::<u64>() as f64 / MB as f64;
                let v = hist.entry(key.clone()).or_default();
                // Footprint halved → the app was restarted; old history no longer describes it.
                if v.last().map(|l| fp < l.1 * 0.5).unwrap_or(false) {
                    v.clear();
                }
                v.push((t, fp));
                v.retain(|(ts, _)| t.saturating_sub(*ts) <= HISTORY_WINDOW);
                let s = st.entry(key.clone()).or_default();
                match trend(v.as_slice(), &cfg, t) {
                    Some(tn) => {
                        s.slope = tn.slope;
                        s.leak = tn.suspect;
                        if tn.suspect && t.saturating_sub(s.last_notify) > HISTORY_WINDOW {
                            s.last_notify = t;
                            if srv {
                                let top = top_proc(cgs.as_slice())
                                    .map(|(pid, n, b)| format!(" Largest process: {n} (PID {pid}, {}).", h(b)))
                                    .unwrap_or_default();
                                let body = format!(
                                    "growing steadily for {:.0} min: +{:.0} MB/h (total +{:.0} MB, now {:.0} MB).{top} Consider restarting it.",
                                    tn.span_min, tn.slope, tn.growth, fp
                                );
                                println!("LEAK SUSPECT: {key} — {body}");
                                if cfg.notify {
                                    notify(&format!("Memory leak suspect: {key}"), &body);
                                }
                            } else {
                                let top = top_proc(cgs.as_slice())
                                    .map(|(pid, n, b)| format!(" Ən böyük proses: {n} (PID {pid}, {}).", h(b)))
                                    .unwrap_or_default();
                                let body = format!(
                                    "{:.0} dəqiqədir dayanmadan artır: +{:.0} MB/saat (cəmi +{:.0} MB, indi {:.0} MB).{top} Yenidən başlatmaq tövsiyə olunur.",
                                    tn.span_min, tn.slope, tn.growth, fp
                                );
                                println!("SIZMA ŞÜBHƏSİ: {} — {body}", disp(key));
                                if cfg.notify {
                                    notify(&format!("Yaddaş sızması şübhəsi: {}", disp(key)), &body);
                                }
                            }
                        }
                    }
                    None => {
                        s.slope = 0.0;
                        s.leak = false;
                    }
                }
            }
            hist.retain(|k, v| apps.contains_key(k) && !v.is_empty());
            st.retain(|k, _| apps.contains_key(k));
            save_history(&p, &hist);
            let mut s = format!("updated={t}\nstarted={started}\nstate={msg}\nreclaimed_total={total}\n");
            for (k, a) in &st {
                if a.reclaimed > 0 || a.leak {
                    s.push_str(&format!("app\t{k}\t{}\t{:.0}\t{}\n", a.reclaimed, a.slope, a.leak as u8));
                }
            }
            write_atomic(&p.run.join("status"), &s);
            if srv {
                write_server_status(&p, &cfg, &apps, &tr, &st, &prot, (code, &msg), g.is_ok(), &mem, started, total);
                // Re-check protection on the next scan: the protect list may have been edited,
                // processes come and go (a postgres container restarted), memory.low changes.
                prot.clear();
            }
        }
        if since_log > 0 && t.saturating_sub(last_log) >= 600 {
            let ps = psi(Path::new("/proc/pressure/memory"));
            if srv {
                println!(
                    "last 10 min: offloaded {} of cold memory | available RAM {} → {} | PSI some10={:.2}",
                    h(since_log),
                    h(avail_at_log),
                    h(mem.avail),
                    ps.some10
                );
            } else {
                println!(
                    "son 10 dəq: {} soyuq yaddaş köçürüldü | boş RAM {} → {} | PSI some10={:.2}",
                    h(since_log),
                    h(avail_at_log),
                    h(mem.avail),
                    ps.some10
                );
            }
            since_log = 0;
            last_log = t;
            avail_at_log = mem.avail;
        }
    }
}

// ───────────────────────────── commands ─────────────────────────────

fn trend_cell(hist: &History, key: &str, cfg: &Config, t: u64) -> (String, bool) {
    match hist.get(key).and_then(|v| trend(v.as_slice(), cfg, t)) {
        Some(tn) if tn.span_min >= 10.0 => (format!("{:+.0}", tn.slope), tn.suspect),
        _ => ("—".into(), false),
    }
}

fn print_system(m: &Mem) {
    let ps = psi(Path::new("/proc/pressure/memory"));
    let (orig, _compr, used) = zram();
    let ratio = if used > 0 { orig as f64 / used as f64 } else { 0.0 };
    println!(
        "RAM {} / {}  boş (available) {} ({:.0}%)  |  zram: {} → {} (x{:.1}, qənaət {})  |  PSI some {:.2}/{:.2}/{:.2} full {:.2}",
        h(m.total.saturating_sub(m.avail)),
        h(m.total),
        h(m.avail),
        pct(m.avail, m.total),
        h(orig),
        h(used),
        ratio,
        h(orig.saturating_sub(used)),
        ps.some10,
        ps.some60,
        ps.some300,
        ps.full10
    );
}

fn sorted(apps: &Apps) -> Vec<(&String, &Vec<Cg>)> {
    let mut list: Vec<(&String, &Vec<Cg>)> = apps.iter().collect();
    list.sort_by_key(|(_, c)| std::cmp::Reverse(c.iter().map(|x| x.current + x.swap).sum::<u64>()));
    list
}

fn analyze() {
    let p = Paths::new();
    let cfg = load_config(&p);
    let m = meminfo();
    let apps = scan(&p.app_slice);
    let hist = load_history(&p);
    let t = now();

    println!("═══ SİSTEM ═══");
    print_system(&m);
    let ps = psi(Path::new("/proc/pressure/memory"));
    let verdict = if ps.some60 < 1.0 {
        "sağlam — proqramlar yaddaş gözləmir"
    } else if ps.some60 < 10.0 {
        "orta — bəzən yaddaş gözləməsi var"
    } else {
        "AĞIR — sistem yaddaş çatışmazlığından ləngiyir"
    };
    println!("Təzyiq qiyməti: {verdict}");

    println!("\n═══ RAM HARA GEDİR ═══");
    let cur = |p: &Path| read_u64(&p.join("memory.current"));
    let rows = [
        ("Tətbiqlər (app.slice)", cur(&p.app_slice)),
        ("Masaüstü sessiyası (plasma, kwin…)", cur(&p.user_root.join("session.slice"))),
        ("Arxa plan xidmətləri (istifadəçi)", cur(&p.user_root.join("background.slice"))),
        ("Sistem xidmətləri (system.slice)", cur(Path::new("/sys/fs/cgroup/system.slice"))),
        ("Kernel (slab, page tables)", m.kernel),
        ("zram-ın tutduğu real RAM", zram().2),
        ("Tam boş (free)", m.free),
    ];
    for (n, b) in rows {
        println!("  {:<38} {:>7}  {:>4.0}%", n, h(b), pct(b, m.total));
    }
    println!("  (disk keşi {} — lazım olanda dərhal boşalır, problem deyil)", h(m.cached));

    println!("\n═══ TƏTBİQLƏR ═══");
    println!("  {:<24} {:>7} {:>8} {:>7} {:>5} {:>8}  QEYD", "TƏTBİQ", "RAM", "ZRAM-DA", "CƏMİ", "NÜS.", "MB/saat");
    let mut leaks = Vec::new();
    for (key, cgs) in sorted(&apps).into_iter().take(15) {
        let ram: u64 = cgs.iter().map(|c| c.current).sum();
        let sw: u64 = cgs.iter().map(|c| c.swap).sum();
        if ram + sw < 20 * MB {
            continue;
        }
        let (tc, leak) = trend_cell(&hist, key, &cfg, t);
        let mut note = String::new();
        if is_protected(&cfg, key) {
            note.push_str("qorunur ");
        }
        if leak {
            note.push_str("⚠ SIZMA ŞÜBHƏSİ ");
            leaks.push(disp(key));
        }
        let live = cgs.iter().filter(|c| c.current + c.swap > MB).count();
        println!("  {:<24} {:>7} {:>8} {:>7} {:>5} {:>8}  {}", disp(key), h(ram), h(sw), h(ram + sw), live, tc, note);
    }

    println!("\n═══ ƏN BÖYÜK PROSESLƏR ═══");
    let mut procs: Vec<(u32, String, u64, u64)> = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
                if let Some((n, r, s)) = proc_mem(pid) {
                    procs.push((pid, n, r, s));
                }
            }
        }
    }
    procs.sort_by_key(|x| std::cmp::Reverse(x.2 + x.3));
    for (pid, n, r, s) in procs.iter().take(8) {
        println!("  {:>8}  {:<20} RAM {:>6}  zram-da {:>6}", pid, n, h(*r), h(*s));
    }

    println!("\n═══ DİAQNOZ ═══");
    let mut tips: Vec<String> = Vec::new();
    let has = |name: &str| procs.iter().any(|x| x.1 == name);
    if !has("earlyoom") && !has("systemd-oomd") {
        tips.push("OOM qoruyucusu yoxdur: RAM tam dolsa sistem dona bilər. → sudo pacman -S earlyoom && sudo systemctl enable --now earlyoom".into());
    }
    if read(Path::new("/sys/kernel/mm/ksm/run")).trim() == "0" {
        tips.push("KSM söndürülüdür: eyni yaddaş səhifələri birləşdirilmir. → sudo pacman -S cachyos-ksm-settings (sonra reboot)".into());
    }
    if m.swap_total == 0 {
        tips.push("Swap/zram yoxdur: soyuq yaddaşı sıxmağa yer yoxdur, optimizer işləyə bilməz.".into());
    } else if pct(m.swap_free, m.swap_total) < 25.0 {
        tips.push(format!(
            "zram-da boş yer azdır ({:.0}%). Ən böyük tətbiqləri yenidən başladın.",
            pct(m.swap_free, m.swap_total)
        ));
    }
    if ps.some60 >= 10.0 {
        tips.push("Yaddaş təzyiqi yüksəkdir: açıq tətbiq/tab sayı bu RAM üçün çoxdur.".into());
    }
    for l in &leaks {
        tips.push(format!("{l}: yaddaşı dayanmadan artır — tətbiqi yenidən başladın (ultra-ram watch ilə izləyin)."));
    }
    let daemon_ok = t.saturating_sub(kv_eq(&read(&p.run.join("status")), "updated")) < 180;
    if !daemon_ok {
        tips.push("ultra-ram daemon işləmir (və ya yeni başlayıb). → systemctl --user enable --now ultra-ram".into());
    } else if hist.values().all(|v| v.len() < 10) {
        tips.push("Trend (MB/saat) üçün hələ məlumat azdır — daemon ən azı 10 dəqiqə işləməlidir.".into());
    }
    if tips.is_empty() {
        println!("  ✓ Problem tapılmadı.");
    }
    for tip in tips {
        println!("  • {tip}");
    }
}

fn optimize(args: &[String]) {
    let dry = args.iter().any(|a| a == "--dry-run");
    let rounds: u32 = args
        .iter()
        .position(|a| a == "--rounds")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
        .clamp(1, 60);
    let p = Paths::new();
    let cfg = load_config(&p);
    let self_cg = self_cgroup();
    let before = meminfo();
    let psi_before = psi(Path::new("/proc/pressure/memory"));
    let z_before = zram();
    if let Err((_, msg)) = gate(&cfg, &before, false) {
        println!("Təhlükəsizlik yoxlaması keçmədi, heç nə edilmədi: {msg}");
        return;
    }
    println!("ƏVVƏL:");
    print_system(&before);
    println!(
        "\n{} dövr, hər biri 3 san{}. Aktiv və təzyiq altında olan tətbiqlərə toxunulmur…",
        rounds,
        if dry { " (SINAQ — heç nə dəyişdirilmir)" } else { "" }
    );

    let mut tr = Tracker::default();
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    pass(&cfg, &scan(&p.app_slice), &mut tr, 3.0, Mode::Observe, &self_cg);
    for _ in 0..rounds {
        let t0 = Instant::now();
        thread::sleep(Duration::from_secs(3));
        // Re-check the gate every round: stop the moment the system shows stress.
        if let Err((_, msg)) = gate(&cfg, &meminfo(), false) {
            println!("Dayandırıldı: {msg}");
            break;
        }
        let apps = scan(&p.app_slice);
        let mode = if dry { Mode::Dry } else { Mode::Real };
        for (k, b) in pass(&cfg, &apps, &mut tr, t0.elapsed().as_secs_f64(), mode, &self_cg) {
            *totals.entry(k).or_insert(0) += b;
        }
    }
    thread::sleep(Duration::from_secs(2));
    let after = meminfo();
    let psi_after = psi(Path::new("/proc/pressure/memory"));
    let z_after = zram();

    println!("\n  {:<24} {:>12}", "TƏTBİQ", if dry { "KÖÇÜRÜLƏRDİ" } else { "KÖÇÜRÜLDÜ" });
    let mut v: Vec<(&String, &u64)> = totals.iter().collect();
    v.sort_by_key(|x| std::cmp::Reverse(*x.1));
    for (k, b) in v {
        println!("  {:<24} {:>12}", disp(k), h(*b));
    }
    let sum: u64 = totals.values().sum();
    println!("  {:<24} {:>12}", "CƏMİ", h(sum));
    let skipped: BTreeSet<String> = tr
        .prev
        .iter()
        .filter(|(_, e)| e.level > 0 || e.c > cfg.active_cpu_pct)
        .filter_map(|(path, _)| path.file_name().map(|n| disp(&app_key(&n.to_string_lossy()))))
        .collect();
    if !skipped.is_empty() {
        println!("  Toxunulmadı (aktiv/təzyiq): {}", skipped.into_iter().collect::<Vec<_>>().join(", "));
    }
    if dry {
        return;
    }
    println!("\nSONRA:");
    print_system(&after);
    let d = after.avail as i64 - before.avail as i64;
    let moved = z_after.0.saturating_sub(z_before.0);
    let cost = z_after.2.saturating_sub(z_before.2);
    println!(
        "\nNƏTİCƏ: zram-a {} köçdü, sıxılmış halda {} tutur → xalis qazanc ≈ {}",
        h(moved),
        h(cost),
        h(moved.saturating_sub(cost))
    );
    println!(
        "        boş RAM {} → {} ({}{} MB; bu rəqəmə eyni anda işləyən proqramlar da təsir edir) | PSI some10 {:.2} → {:.2}",
        h(before.avail),
        h(after.avail),
        if d >= 0 { "+" } else { "" },
        d / MB as i64,
        psi_before.some10,
        psi_after.some10
    );
    println!("Geri qaytarma lazım deyil: köçürülən səhifələr tətbiq onlara toxunan kimi özü geri gəlir.");
}

fn watch() {
    let p = Paths::new();
    let self_cg = self_cgroup();
    let mut tr = Tracker::default();
    let mut last = Instant::now();
    let (mut hist, mut hist_at) = (load_history(&p), Instant::now());
    let mut cfg = load_config(&p);
    loop {
        let apps = scan(&p.app_slice);
        let dt = last.elapsed().as_secs_f64();
        last = Instant::now();
        pass(&cfg, &apps, &mut tr, dt, Mode::Observe, &self_cg);
        if hist_at.elapsed().as_secs() >= 30 {
            hist = load_history(&p);
            cfg = load_config(&p);
            hist_at = Instant::now();
        }
        let t = now();
        let status = read(&p.run.join("status"));
        let alive = t.saturating_sub(kv_eq(&status, "updated")) < 180;
        let dstate = status.lines().find_map(|l| l.strip_prefix("state=")).unwrap_or("");
        print!("\x1b[H\x1b[2J");
        println!("ULTRA RAM — canlı izləmə (çıxış: Ctrl+C)\n");
        print_system(&meminfo());
        if alive {
            println!("Daemon: {dstate} | bu sessiyada köçürülüb: {}", h(kv_eq(&status, "reclaimed_total")));
        } else {
            println!("Daemon: İŞLƏMİR (systemctl --user enable --now ultra-ram)");
        }
        println!(
            "\n  {:<24} {:>7} {:>8} {:>6} {:>8} {:>8}  VƏZİYYƏT",
            "TƏTBİQ", "RAM", "ZRAM-DA", "CPU%", "TƏZYİQ%", "MB/saat"
        );
        for (key, cgs) in sorted(&apps).into_iter().take(18) {
            let ram: u64 = cgs.iter().map(|c| c.current).sum();
            let sw: u64 = cgs.iter().map(|c| c.swap).sum();
            if ram + sw < 20 * MB {
                continue;
            }
            let (mut cpu, mut pr, mut backoff) = (0.0f64, 0.0f64, false);
            for c in cgs.iter() {
                if let Some(e) = tr.prev.get(&c.path) {
                    cpu += e.c;
                    pr = pr.max(e.p);
                    backoff |= e.level > 0 && tr.tick < e.skip_until;
                }
            }
            let (tc, leak) = trend_cell(&hist, key, &cfg, t);
            let state = if leak {
                "⚠ SIZMA ŞÜBHƏSİ"
            } else if is_protected(&cfg, key) {
                "qorunur"
            } else if pr >= cfg.psi_threshold_pct || backoff {
                "təzyiq — toxunulmur"
            } else if cpu > cfg.active_cpu_pct {
                "aktiv — toxunulmur"
            } else if ram < cfg.min_app_mb * MB {
                "kiçik"
            } else {
                "sakit"
            };
            println!("  {:<24} {:>7} {:>8} {:>6.0} {:>8.2} {:>8}  {}", disp(key), h(ram), h(sw), cpu, pr, tc, state);
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn status() {
    let p = Paths::new();
    let s = read(&p.run.join("status"));
    let t = now();
    let upd = kv_eq(&s, "updated");
    if upd == 0 || t.saturating_sub(upd) > 180 {
        println!("Daemon: İŞLƏMİR (və ya 1 dəqiqədən az işləyir)  →  systemctl --user status ultra-ram");
    } else {
        let up = t.saturating_sub(kv_eq(&s, "started")) / 60;
        println!("Daemon: işləyir ({} saat {} dəq)", up / 60, up % 60);
        println!("Vəziyyət: {}", s.lines().find_map(|l| l.strip_prefix("state=")).unwrap_or("?"));
        println!("Bu müddətdə zram-a köçürülən soyuq yaddaş: {}", h(kv_eq(&s, "reclaimed_total")));
        for l in s.lines().filter(|l| l.starts_with("app\t")) {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() >= 5 {
                let leak = if f[4] == "1" { "  ⚠ SIZMA ŞÜBHƏSİ" } else { "" };
                println!(
                    "  {:<24} köçürülüb {:>7}  trend {:>5} MB/saat{}",
                    disp(f[1]),
                    h(f[2].parse().unwrap_or(0)),
                    f[3],
                    leak
                );
            }
        }
    }
    if p.run.join("paused").exists() {
        println!("FASİLƏ: aktivdir (ultra-ram resume)");
    }
    for (k, mb) in &load_limits(&p) {
        println!("Limit: {} ≤ {} MB (hər nüsxə üçün, yumşaq)", disp(k), mb);
    }
    println!();
    print_system(&meminfo());
}

/// `ultra-ram workloads`: read-only list of what the server profile would manage, and why a
/// workload is protected. Uses /etc/ultra-ram/config (or the server defaults).
fn list_workloads() {
    let p = Paths::for_profile(Profile::Server);
    let cfg = load_config(&p);
    let cgroot = PathBuf::from(server::CGROUP_ROOT);
    let mut prot = ProtCache::new();
    let apps = scan_server(&cfg, &cgroot, &self_cgroup(), &mut prot);
    println!("roots: {} (depth {})", cfg.roots.join(", "), cfg.discover_depth);
    println!("  {:<56} {:>7} {:>7}  NOTE", "WORKLOAD", "RAM", "SWAP");
    for (key, cgs) in sorted(&apps) {
        for cg in cgs {
            let note = match prot.get(&cg.path).cloned().flatten() {
                Some(r) => format!("protected ({r})"),
                None if cg.current < cfg.min_app_mb * MB => format!("below min_app_mb ({})", cfg.min_app_mb),
                None => String::new(),
            };
            println!("  {:<56} {:>7} {:>7}  {}", key, h(cg.current), h(cg.swap), note);
        }
    }
    if apps.is_empty() {
        println!("  (no workload found)");
    }
}

/// `ultra-ram status --json` / `status --profile server`: what the server daemon wrote last.
fn status_server(json: bool) -> i32 {
    let f = Path::new(server::RUN_DIR).join(if json { "status.json" } else { "status.txt" });
    match fs::read_to_string(&f) {
        Ok(s) => {
            let age = fs::metadata(&f).ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok());
            if age.is_some_and(|a| a.as_secs() > 180) {
                eprintln!("warning: {} is {} s old — is ultra-ram-server.service running?", f.display(), age.unwrap_or_default().as_secs());
            }
            print!("{s}");
            0
        }
        Err(e) => {
            eprintln!("no server status at {} ({e}) — systemctl status ultra-ram-server", f.display());
            1
        }
    }
}

fn limit(args: &[String]) {
    let p = Paths::new();
    let apps = scan(&p.app_slice);
    let (Some(name), Some(mb)) = (args.first(), args.get(1).and_then(|v| v.parse::<u64>().ok())) else {
        println!("İstifadə: ultra-ram limit <tətbiq> <MB>");
        return;
    };
    if mb < 256 {
        println!("Təhlükəsizlik: 256 MB-dan aşağı limit qoyulmur (tətbiq donar).");
        return;
    }
    let key = match resolve_app(&apps, name) {
        Ok(k) => k,
        Err(e) => {
            println!("{e}");
            return;
        }
    };
    let biggest = apps[&key].iter().map(|c| c.current).max().unwrap_or(0);
    if biggest > mb * MB {
        println!(
            "Diqqət: {} hazırda {} işlədir — limit bundan aşağıdır, artıq hissə zram-a sıxılacaq və tətbiq yavaşlaya bilər.",
            disp(&key),
            h(biggest)
        );
    }
    let mut l = load_limits(&p);
    l.insert(key.clone(), mb);
    save_limits(&p, &l);
    enforce_limits(&p, &apps);
    println!("✓ {} üçün yumşaq limit: {} MB (memory.high). Ləğv: ultra-ram unlimit {}", disp(&key), mb, disp(&key));
}

fn unlimit(args: &[String]) {
    let p = Paths::new();
    let apps = scan(&p.app_slice);
    let Some(name) = args.first() else {
        println!("İstifadə: ultra-ram unlimit <tətbiq>");
        return;
    };
    let mut l = load_limits(&p);
    let n = name.to_lowercase();
    let found = l
        .keys()
        .find(|k| k.to_lowercase() == n || disp(k).to_lowercase() == n || k.to_lowercase().contains(&n))
        .cloned();
    let Some(key) = found else {
        println!("'{name}' üçün limit yoxdur.");
        return;
    };
    l.remove(&key);
    save_limits(&p, &l);
    let c = reset_high(&apps, &key);
    println!("✓ {} limiti götürüldü ({} cgroup sıfırlandı).", disp(&key), c);
}

fn rollback() {
    let p = Paths::new();
    let _ = fs::create_dir_all(&p.run);
    let _ = fs::write(p.run.join("paused"), "");
    let apps = scan(&p.app_slice);
    let l = load_limits(&p);
    let mut n = 0;
    for k in l.keys() {
        n += reset_high(&apps, k);
    }
    save_limits(&p, &BTreeMap::new());
    println!("✓ Geri qaytarıldı:");
    println!("  • avtomatik köçürmə dayandırıldı (fasilə)");
    println!("  • {} limit silindi, {} cgroup memory.high=max edildi", l.len(), n);
    println!("  • zram-dakı səhifələr tətbiqlər istifadə etdikcə özü RAM-a qayıdır — məlumat itkisi yoxdur");
    println!("Yenidən işə salmaq: ultra-ram resume | Tam silmək: ./install.sh --uninstall");
}

fn help() {
    println!(
        "ultra-ram — ağıllı RAM idarəetməsi (PSI əsaslı proaktiv reclaim)

  analyze             Dərin analiz: RAM hara gedir, tətbiqlər, sızma şübhələri, diaqnoz
  watch               Canlı izləmə (2 saniyədən bir)
  optimize            Bir dəfəlik təhlükəsiz optimallaşdırma, əvvəl/sonra ölçmə ilə
      --dry-run       Heç nə dəyişmədən nə ediləcəyini göstər
      --rounds N      Dövr sayı (standart 8)
  status              Daemon vəziyyəti və statistika
  pause / resume      Avtomatik rejimi dayandır / davam etdir
  limit <app> <MB>    Tətbiqə yumşaq RAM limiti (artığı zram-a sıxılır, öldürülmür)
  unlimit <app>       Limiti götür
  rollback            Hər şeyi geri qaytar: limitləri sil, avtomatikanı dayandır
  daemon              Arxa plan xidməti (systemd tərəfindən işə salınır)
  zram-tier ...       [root, istəyə bağlı] lz4 zram səviyyəsi + boş səhifələrin zstd ilə
                      yenidən sıxılması: setup | maintain | status | remove (ətraflı: zram-tier help)
  prefetch ...        [root, istəyə bağlı] tətbiqə qayıdanda zram-dakı yaddaşını qabaqcadan,
                      paralel RAM-a qaytar: run | once <ad> (ətraflı: prefetch help)
  age ...             [root, istəyə bağlı] tətbiqlərin yaddaşını MGLRU ilə mütəmadi yaşlandır,
                      daemon yalnız həqiqətən soyuq yaddaşı köçürsün: run | status (ətraflı: age help)

Ayarlar: ~/.config/ultra-ram/config

Server profile (root; services, containers, VMs, pods — see README):
  daemon --profile server        system service ultra-ram-server.service, config /etc/ultra-ram/config
  status --profile server        what it did, per workload  (status --json: /run/ultra-ram/status.json)
  pause | resume --profile server
  workloads                      read-only: the workloads it would manage, and which are protected
  print-default-config --profile server
  age run --profile server | prefetch run --profile server   helpers on the same workloads"
    );
}

/// The server workload set for the root helpers: only with an explicit `--profile server`, so a
/// desktop's helpers never change scope. Roots and depth come from /etc/ultra-ram/config.
fn helper_discovery(cli: Option<Profile>) -> Option<server::Discovery> {
    (cli == Some(Profile::Server)).then(|| load_config(&Paths::for_profile(Profile::Server)).discovery())
}

fn main() {
    let raw: Vec<String> = env::args().skip(1).collect();
    let (args, cli_profile) = match split_profile(&raw) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("✗ {e}");
            std::process::exit(2);
        }
    };
    let rest: &[String] = if args.len() > 1 { &args[1..] } else { &[] };
    let profile = || resolve_profile(cli_profile, is_root(), &read(&Path::new(server::CFG_DIR).join("config")));
    match args.first().map(|s| s.as_str()) {
        Some("daemon") => daemon(profile()),
        Some("analyze") => analyze(),
        Some("watch") => watch(),
        Some("optimize") => optimize(rest),
        Some("status") if rest.iter().any(|a| a == "--json") => std::process::exit(status_server(true)),
        Some("status") if profile() == Profile::Server => std::process::exit(status_server(false)),
        Some("status") => status(),
        Some("workloads") => list_workloads(),
        Some("limit") => limit(rest),
        Some("unlimit") => unlimit(rest),
        Some("rollback") => rollback(),
        Some("pause") => {
            let p = Paths::for_profile(profile());
            let _ = fs::create_dir_all(&p.run);
            let _ = fs::write(p.run.join("paused"), "");
            if p.profile == Profile::Server {
                println!("✓ Paused: no reclaim until 'ultra-ram resume --profile server' (status keeps updating).");
            } else {
                println!("✓ Fasilə: avtomatik köçürmə dayandırıldı (izləmə və sızma aşkarlanması davam edir).");
            }
        }
        Some("resume") => {
            let p = Paths::for_profile(profile());
            let _ = fs::remove_file(p.run.join("paused"));
            if p.profile == Profile::Server {
                println!("✓ Resumed.");
            } else {
                println!("✓ Davam edir.");
            }
        }
        Some("print-default-config") if cli_profile == Some(Profile::Server) => print!("{DEFAULT_SERVER_CONFIG}"),
        Some("print-default-config") => print!("{DEFAULT_CONFIG}"),
        Some("zram-tier") => std::process::exit(zram_tier::run(rest)),
        Some("prefetch") => std::process::exit(prefetch::run(rest, helper_discovery(cli_profile))),
        Some("age") => std::process::exit(age::run(rest, helper_discovery(cli_profile))),
        _ => help(),
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn profile_parsing_and_resolution() {
        assert_eq!(Profile::parse(" Server "), Some(Profile::Server));
        assert_eq!(Profile::parse("desktop"), Some(Profile::Desktop));
        assert_eq!(Profile::parse("laptop"), None);
        assert_eq!(declared_profile("# profile = server\nprofile = server # root\n"), Some(Profile::Server));
        assert_eq!(declared_profile("profiles = server\n"), None);
        assert_eq!(declared_profile(""), None);
        // --profile wins; otherwise only root with a server /etc config gets the server profile.
        assert_eq!(resolve_profile(Some(Profile::Server), false, ""), Profile::Server);
        assert_eq!(resolve_profile(Some(Profile::Desktop), true, "profile = server"), Profile::Desktop);
        assert_eq!(resolve_profile(None, true, "profile = server"), Profile::Server);
        assert_eq!(resolve_profile(None, false, "profile = server"), Profile::Desktop);
        assert_eq!(resolve_profile(None, true, "profile = desktop"), Profile::Desktop);
        assert_eq!(resolve_profile(None, true, ""), Profile::Desktop);

        assert_eq!(split_profile(&sv(&["daemon", "--profile", "server"])), Ok((sv(&["daemon"]), Some(Profile::Server))));
        assert_eq!(split_profile(&sv(&["age", "run", "--profile=server", "-v"])), Ok((sv(&["age", "run", "-v"]), Some(Profile::Server))));
        assert_eq!(split_profile(&sv(&["status"])), Ok((sv(&["status"]), None)));
        assert!(split_profile(&sv(&["daemon", "--profile"])).is_err());
        assert!(split_profile(&sv(&["daemon", "--profile", "cloud"])).is_err());
    }

    #[test]
    fn config_parsing_by_profile() {
        // Desktop defaults are unchanged.
        let d = parse_config("", Profile::Desktop);
        assert_eq!(d.profile, Profile::Desktop);
        assert_eq!((d.activate_avail_pct, d.max_busy_cpu_pct, d.active_cpu_pct), (40.0, 60.0, 25.0));
        assert_eq!(d.protect, sv(&["gamescope", "wine", "lutris", "heroic"]));
        assert!(d.notify);
        // The shipped desktop config parses back to the defaults.
        let shipped = parse_config(DEFAULT_CONFIG, Profile::Desktop);
        assert_eq!((shipped.activate_avail_pct, shipped.max_busy_cpu_pct, shipped.cold_age_secs), (40.0, 60.0, 120));

        let s = parse_config("", Profile::Server);
        assert_eq!(s.profile, Profile::Server);
        assert_eq!((s.activate_avail_pct, s.max_busy_cpu_pct), (OFF_PCT, OFF_PCT));
        assert!(s.active_cpu_pct.is_infinite());
        assert!(!s.notify && s.respect_memory_low);
        assert!(s.protect.contains(&"postgres".to_string()) && s.protect.contains(&"systemd-journald".to_string()));
        assert_eq!(s.discovery(), server::Discovery::default());
        // The shipped server config is the server defaults, written out.
        let shipped = parse_config(DEFAULT_SERVER_CONFIG, Profile::Server);
        assert_eq!(declared_profile(DEFAULT_SERVER_CONFIG), Some(Profile::Server));
        assert_eq!(shipped.roots, s.roots);
        assert_eq!(shipped.protect, s.protect);
        assert_eq!((shipped.activate_avail_pct, shipped.max_busy_cpu_pct), (OFF_PCT, OFF_PCT));
        assert!(shipped.active_cpu_pct.is_infinite());
        assert_eq!((shipped.psi_threshold_pct, shipped.max_offload_pct, shipped.discover_depth), (1.5, 50.0, 4));
        assert_eq!((shipped.psi_threshold_pct, shipped.refault_tolerance_pct, shipped.cold_age_secs), (s.psi_threshold_pct, s.refault_tolerance_pct, s.cold_age_secs));
        assert_eq!((s.refault_tolerance_pct, s.cold_age_secs), (50.0, 300));
        assert_eq!(Config::for_profile(Profile::Desktop).refault_tolerance_pct, 12.5);

        // Operator overrides: gates back on, roots, depth clamped, junk roots dropped.
        let o = parse_config(
            "activate_avail_pct = 30\nmax_busy_cpu_pct = 90\nactive_cpu_pct = 200\nroots = /system.slice/, ../x, user.slice, docker\n\
             discover_depth = 99\nrespect_memory_low = no\nprotect = Nginx, , redis\n",
            Profile::Server,
        );
        assert_eq!((o.activate_avail_pct, o.max_busy_cpu_pct, o.active_cpu_pct), (30.0, 90.0, 200.0));
        assert_eq!(o.roots, sv(&["system.slice", "docker"]));
        assert_eq!(o.discover_depth, server::MAX_DEPTH);
        assert!(!o.respect_memory_low);
        assert_eq!(o.protect, sv(&["nginx", "redis"]));
        // Only invalid roots: keep the defaults rather than watch nothing.
        assert_eq!(parse_config("roots = ../a, user.slice", Profile::Server).roots, s.roots);
        // `off` works on the desktop too, without changing its defaults otherwise.
        let x = parse_config("max_busy_cpu_pct = off\nactive_cpu_pct = OFF", Profile::Desktop);
        assert_eq!(x.max_busy_cpu_pct, OFF_PCT);
        assert!(x.active_cpu_pct.is_infinite());
        assert_eq!(x.activate_avail_pct, 40.0);
    }

    fn mem(avail_pct: u64, swap_free_pct: u64) -> Mem {
        let total = 4 * GB;
        Mem { total, avail: total / 100 * avail_pct, swap_total: GB, swap_free: GB / 100 * swap_free_pct, ..Default::default() }
    }

    #[test]
    fn gate_selection_by_profile() {
        let d = Config::for_profile(Profile::Desktop);
        let s = Config::for_profile(Profile::Server);
        // A busy machine with lots of free RAM: the desktop waits, the server works.
        let m = mem(80, 90);
        assert_eq!(tick_gate(&d, &m, 95.0, false, 0, 0.0), Err(Gate::Busy(95.0)));
        assert_eq!(tick_gate(&d, &mem(80, 90), 10.0, false, 0, 0.0).map_err(|g| g.code()), Err("enough"));
        assert_eq!(tick_gate(&s, &m, 100.0, false, 0, 0.0), Ok(()));
        assert_eq!(tick_gate(&s, &mem(100, 90), 100.0, false, 0, 0.0), Ok(()));
        // The safety gates stay for both: pause, swap, zram size, global PSI.
        for c in [&d, &s] {
            let low = mem(20, 90);
            assert_eq!(tick_gate(c, &low, 0.0, true, 0, 0.0), Err(Gate::Paused));
            assert_eq!(tick_gate(c, &mem(20, 10), 0.0, false, 0, 0.0).map_err(|g| g.code()), Err("swapfull"));
            assert_eq!(tick_gate(c, &low, 0.0, false, GB * 2, 0.0).map_err(|g| g.code()), Err("zramfull"));
            assert_eq!(tick_gate(c, &low, 0.0, false, 0, 25.0), Err(Gate::Pressure(25.0)));
            let noswap = Mem { swap_total: 0, swap_free: 0, ..low };
            assert_eq!(tick_gate(c, &noswap, 0.0, false, 0, 0.0), Err(Gate::NoSwap));
            assert_eq!(tick_gate(c, &low, 0.0, false, 0, 0.0), Ok(()));
        }
        // An operator can turn the desktop gates back on for a server.
        let s2 = parse_config("activate_avail_pct = 30\nmax_busy_cpu_pct = 70", Profile::Server);
        assert_eq!(tick_gate(&s2, &mem(20, 90), 80.0, false, 0, 0.0), Err(Gate::Busy(80.0)));
        assert_eq!(tick_gate(&s2, &mem(50, 90), 10.0, false, 0, 0.0).map_err(|g| g.code()), Err("enough"));
        // Desktop messages are the original Azerbaijani ones; the server speaks English.
        assert_eq!(Gate::Busy(70.0).msg(Profile::Desktop), "sistem məşğuldur (CPU 70%) — köçürmə təxirə salındı");
        assert_eq!(Gate::Paused.msg(Profile::Desktop), "fasilədədir (ultra-ram resume ilə davam etdirin)");
        assert_eq!(Gate::Pressure(12.34).msg(Profile::Server), "system under memory pressure (PSI 12.3) — standing down");
        assert_eq!(active_msg(Profile::Desktop), "aktiv — soyuq yaddaş zram-a köçürülür");
    }

    #[test]
    fn server_pass_skips_protected_and_busy_is_not_active() {
        let s = Config::for_profile(Profile::Server);
        let cg = |name: &str, protected: bool| Cg {
            path: PathBuf::from(format!("/nonexistent/system.slice/{name}")),
            current: 500 * MB,
            swap: 0,
            footprint: 500 * MB,
            psi_total: 0,
            cpu_usec: 0,
            refault_anon: 0,
            psi10: 0.0,
            protected,
        };
        let mut apps = Apps::new();
        apps.insert("a".into(), vec![cg("web-1.service", false)]);
        apps.insert("b".into(), vec![cg("postgres-bench.service", true)]);
        let mut tr = Tracker::default();
        // Baseline tick, then a tick in which web-1 burned 3 cores: still a reclaim candidate on a
        // server (Dry mode: plans without touching anything).
        pass(&s, &apps, &mut tr, 15.0, Mode::Observe, Path::new("/self"));
        for c in apps.get_mut("a").unwrap() {
            c.cpu_usec = 45_000_000;
        }
        let out = pass(&s, &apps, &mut tr, 15.0, Mode::Dry, Path::new("/self"));
        assert!(out.get("a").is_some_and(|b| *b > 0), "{out:?}");
        assert!(!out.contains_key("b"), "protected workloads are never reclaimed from");
        let e = tr.prev.get(&apps["a"][0].path);
        assert_eq!(workload_state(&s, &apps["a"][0], e, tr.tick, true), "reclaiming");
        assert_eq!(workload_state(&s, &apps["b"][0], tr.prev.get(&apps["b"][0].path), tr.tick, true), "protected");
        assert_eq!(workload_state(&s, &apps["a"][0], None, tr.tick, true), "new");
        // The same CPU burn makes the app "active" (skipped) on the desktop.
        let d = Config::for_profile(Profile::Desktop);
        let mut tr = Tracker::default();
        let mut apps_d = Apps::new();
        apps_d.insert("a".into(), vec![cg("web-1.service", false)]);
        pass(&d, &apps_d, &mut tr, 15.0, Mode::Observe, Path::new("/self"));
        apps_d.get_mut("a").unwrap()[0].cpu_usec = 45_000_000;
        assert!(pass(&d, &apps_d, &mut tr, 15.0, Mode::Dry, Path::new("/self")).is_empty());
    }

    #[test]
    fn server_scan_end_to_end() {
        let root = std::env::temp_dir().join(format!("ultra-ram-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let put = |rel: &str, name: &str, v: &str| {
            let d = root.join(rel);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(name), v).unwrap();
        };
        put("system.slice/web-1.service", "memory.current", "524288000\n");
        put("system.slice/web-1.service", "memory.low", "0\n");
        put("system.slice/api.service", "memory.low", "1073741824\n");
        put("system.slice/postgres-bench.service", "memory.current", "1\n");
        put("system.slice/ultra-ram-server.service", "memory.current", "1\n");
        put("user.slice/user-1000.slice", "memory.current", "1\n");
        let cfg = Config::for_profile(Profile::Server);
        let mut prot = ProtCache::new();
        let me = root.join("system.slice/ultra-ram-server.service");
        let apps = scan_server(&cfg, &root, &me, &mut prot);
        assert_eq!(apps.keys().cloned().collect::<Vec<_>>(), sv(&["system.slice/api.service", "system.slice/postgres-bench.service", "system.slice/web-1.service"]));
        assert_eq!(apps["system.slice/web-1.service"][0].current, 500 * MB);
        assert!(!apps["system.slice/web-1.service"][0].protected);
        assert!(apps["system.slice/api.service"][0].protected);
        assert!(apps["system.slice/postgres-bench.service"][0].protected);
        assert_eq!(prot.get(&root.join("system.slice/api.service")), Some(&Some("memory.low".to_string())));
        let _ = fs::remove_dir_all(&root);
    }
}
