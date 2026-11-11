//! Optional "zram tier": a dedicated zram swap device that compresses freshly
//! offloaded pages with a fast algorithm (lz4) and, later, recompresses pages that
//! stayed untouched for a long time with a stronger one (zstd).
//!
//! Like `prefetch`, this part of ultra-ram needs root, so it lives in separate
//! subcommands (`ultra-ram zram-tier ...`); the reclaim daemon itself stays rootless
//! and simply benefits from whichever swap device has the highest priority.
//!
//! Kernel interface (Documentation/admin-guide/blockdev/zram.rst, drivers/block/zram):
//! * `/sys/class/zram-control/hot_add` (read) creates a device and returns its index;
//!   `hot_remove` (write index) deletes it (-EBUSY while it is still swapped on).
//! * `comp_algorithm` and `recomp_algorithm` (`algo=NAME priority=N`, needs
//!   CONFIG_ZRAM_MULTI_COMP) must be written before `disksize`; afterwards -EBUSY.
//! * `idle`: `all` marks every allocated slot idle; a number of seconds (needs
//!   CONFIG_ZRAM_TRACK_ENTRY_ACTIME, else -EINVAL) marks slots not accessed for that
//!   long as idle and clears the mark on younger ones. Any access clears the mark.
//! * `recompress`: `type=idle priority=1 [threshold=BYTES]` recompresses idle slots
//!   with the secondary algorithm. Slots that do not shrink are flagged
//!   incompressible and skipped afterwards; already-recompressed slots are skipped.
//!
//! Safety rules: only the device recorded in our state file (and fingerprinted by
//! boot id, disksize and algorithm) is ever written to, swapped off or removed.
//! setup, remove and every maintain pass hold an exclusive flock on LOCK_FILE, so they
//! never interleave (e.g. a manual `remove` during a recompress pass of the service).

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STATE_FILE: &str = "/run/ultra-ram-zram-tier";
const LOCK_FILE: &str = "/run/ultra-ram-zram-tier.lock";
const DEFAULT_IDLE_SECS: u64 = 1800;
const DEFAULT_SIZE_PCT: u64 = 50;
const DEFAULT_ALGO: &str = "lz4";
const DEFAULT_RECOMP_ALGO: &str = "zstd";
/// Priority slot of the secondary algorithm (1 = first alternative).
const RECOMP_PRIO: u32 = 1;
/// New swap priority = highest existing + this.
const PRIO_STEP: i32 = 10;
/// swapon(2) accepts 0..=32767 for an explicit priority.
const PRIO_MAX: i32 = 32767;
/// Priority used when no swap device exists yet.
const PRIO_NO_SWAP: i32 = 100;
const EINVAL: i32 = 22;
const EINTR: i32 = 4;
/// EAGAIN/EWOULDBLOCK on x86_64 and arm64.
const EWOULDBLOCK: i32 = 11;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

extern "C" {
    /// libc flock(2); std links libc but has no wrapper for it.
    fn flock(fd: i32, op: i32) -> i32;
}

// ───────────────────────────── paths ─────────────────────────────

/// Every filesystem location this module touches, so tests can point it at a temp dir.
pub struct Sys {
    block: PathBuf,        // /sys/block
    zram_control: PathBuf, // /sys/class/zram-control
    proc_: PathBuf,        // /proc
    zswap_enabled: PathBuf,
    debugfs_zram: PathBuf, // /sys/kernel/debug/zram
    dev: PathBuf,          // /dev
    state: PathBuf,
    lock: PathBuf,
}

impl Sys {
    pub fn real() -> Sys {
        Sys {
            block: "/sys/block".into(),
            zram_control: "/sys/class/zram-control".into(),
            proc_: "/proc".into(),
            zswap_enabled: "/sys/module/zswap/parameters/enabled".into(),
            debugfs_zram: "/sys/kernel/debug/zram".into(),
            dev: "/dev".into(),
            state: STATE_FILE.into(),
            lock: LOCK_FILE.into(),
        }
    }
    #[cfg(test)]
    fn under(root: &Path) -> Sys {
        Sys {
            block: root.join("sys/block"),
            zram_control: root.join("sys/class/zram-control"),
            proc_: root.join("proc"),
            zswap_enabled: root.join("sys/module/zswap/parameters/enabled"),
            debugfs_zram: root.join("sys/kernel/debug/zram"),
            dev: root.join("dev"),
            state: root.join("run/ultra-ram-zram-tier"),
            lock: root.join("run/ultra-ram-zram-tier.lock"),
        }
    }
    fn zdir(&self, i: u32) -> PathBuf {
        self.block.join(format!("zram{i}"))
    }
    fn node(&self, i: u32) -> PathBuf {
        self.dev.join(format!("zram{i}"))
    }
    /// `/dev/zramN` (a /proc/swaps name) → N.
    fn zram_index(&self, name: &str) -> Option<u32> {
        let n = name.strip_prefix(&format!("{}/zram", self.dev.display()))?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        n.parse().ok()
    }
}

// ───────────────────────────── lock ─────────────────────────────

/// Exclusive flock on `Sys::lock`; released when dropped (the fd is closed, also on a crash).
struct Lock {
    _f: fs::File,
}

fn flock_retry(fd: i32, op: i32) -> io::Result<()> {
    loop {
        // SAFETY: flock takes no pointers; fd belongs to a File that outlives the call.
        if unsafe { flock(fd, op) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(EINTR) {
            return Err(e);
        }
    }
}

/// Ok(None): another setup/remove/maintain holds the lock right now.
fn try_lock(sys: &Sys) -> Result<Option<Lock>, String> {
    let f = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(&sys.lock)
        .map_err(|e| format!("kilid faylı {} açılmadı: {e}", sys.lock.display()))?;
    match flock_retry(f.as_raw_fd(), LOCK_EX | LOCK_NB) {
        Ok(()) => Ok(Some(Lock { _f: f })),
        Err(e) if e.raw_os_error() == Some(EWOULDBLOCK) => Ok(None),
        Err(e) => Err(format!("{} kilidlənmədi: {e}", sys.lock.display())),
    }
}

/// Waits for the lock (a recompress pass can take a while).
fn lock_wait(sys: &Sys) -> Result<Lock, String> {
    if let Some(l) = try_lock(sys)? {
        return Ok(l);
    }
    println!("• Başqa zram-tier əməliyyatı gedir (məs. yenidən sıxma keçidi) — bitməsi gözlənilir…");
    let f = OpenOptions::new()
        .write(true)
        .open(&sys.lock)
        .map_err(|e| format!("kilid faylı {} açılmadı: {e}", sys.lock.display()))?;
    flock_retry(f.as_raw_fd(), LOCK_EX).map_err(|e| format!("{} kilidlənmədi: {e}", sys.lock.display()))?;
    Ok(Lock { _f: f })
}

/// Runs an external program (mkswap, swapon, swapoff, modprobe). Injected for tests.
type Runner<'a> = &'a dyn Fn(&str, &[String]) -> Result<(), String>;

fn run_cmd(prog: &str, args: &[String]) -> Result<(), String> {
    match Command::new(prog).args(args).output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Err(format!("{prog} {}: {} {}", args.join(" "), o.status, err))
        }
        Err(e) => Err(format!("{prog} işə salına bilmədi: {e}")),
    }
}

// ───────────────────────────── small io helpers ─────────────────────────────

fn read(p: &Path) -> io::Result<String> {
    fs::read_to_string(p)
}

/// Writes one value to an existing sysfs attribute (never creates files).
fn sysfs_write(p: &Path, val: &str) -> io::Result<()> {
    let mut f = OpenOptions::new().write(true).truncate(true).open(p)?;
    f.write_all(val.as_bytes())
}

fn wr(p: &Path, val: &str) -> Result<(), String> {
    sysfs_write(p, val).map_err(|e| format!("'{val}' → {} yazıla bilmədi: {e}", p.display()))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hb(b: u64) -> String {
    const MB: u64 = 1 << 20;
    const GB: u64 = 1 << 30;
    if b >= GB {
        format!("{:.2}G", b as f64 / GB as f64)
    } else {
        format!("{}M", b / MB)
    }
}

// ───────────────────────────── pure parsers ─────────────────────────────

/// `lzo-rle lzo lz4 [zstd]` → (all names, selected one).
fn parse_algo_list(s: &str) -> (Vec<String>, Option<String>) {
    let mut all = Vec::new();
    let mut sel = None;
    for t in s.split_whitespace() {
        if let Some(n) = t.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
            all.push(n.to_string());
            sel = Some(n.to_string());
        } else {
            all.push(t.to_string());
        }
    }
    (all, sel)
}

/// `recomp_algorithm` output: one `#PRIO: a b [c]` line per configured priority.
fn parse_recomp_list(s: &str) -> Vec<(u32, String)> {
    s.lines()
        .filter_map(|l| {
            let l = l.trim().strip_prefix('#')?;
            let (p, rest) = l.split_once(':')?;
            Some((p.trim().parse().ok()?, parse_algo_list(rest).1?))
        })
        .collect()
}

#[derive(Debug, PartialEq, Clone)]
struct Swap {
    name: String,
    size_kb: u64,
    used_kb: u64,
    prio: i32,
}

/// /proc/swaps: `Filename Type Size Used Priority` (sizes in KiB, name space-escaped as \040).
fn parse_swaps(s: &str) -> Vec<Swap> {
    s.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 5 {
                return None;
            }
            Some(Swap {
                name: f[0].to_string(),
                size_kb: f[2].parse().ok()?,
                used_kb: f[3].parse().ok()?,
                prio: f[4].parse().ok()?,
            })
        })
        .collect()
}

/// Strictly above every existing swap device, so new swap-outs land on the tier first.
fn pick_priority(swaps: &[Swap]) -> i32 {
    match swaps.iter().map(|s| s.prio).max() {
        None => PRIO_NO_SWAP,
        Some(m) => m.saturating_add(PRIO_STEP).clamp(0, PRIO_MAX),
    }
}

/// Can a swapoff pull `need` bytes back into RAM and still leave 10% of MemTotal spare?
/// What absorbs them: MemAvailable, the tier's own RAM (`tier_mem`, mm_stat mem_used_total,
/// released as the device drains) and free *disk* swap that reclaim can push pages out to.
/// Free space on other zram devices does not count: storing pages there costs RAM again.
/// The kernel itself does not check this (swapoff only does an overcommit check on the
/// device size), it just keeps pulling pages in under rising pressure.
fn swapoff_fits(need: u64, avail: u64, tier_mem: u64, disk_free: u64, memtotal: u64) -> bool {
    need.saturating_add(memtotal / 10) <= avail.saturating_add(tier_mem).saturating_add(disk_free)
}

/// Does `cand` (a zram swap device without a state file of ours) look like a tier that an
/// earlier `setup` left behind? Deliberately conservative — all of: it sits strictly above at
/// least one other swap device, its primary algorithm is ours and its recompression slot
/// RECOMP_PRIO holds our secondary algorithm. A distro zram0 normally fails the first or
/// last test (alone or at the bottom; no recomp_algorithm configured).
fn looks_like_leftover(
    cand: &Swap,
    all: &[Swap],
    comp_sel: Option<&str>,
    recomp: &[(u32, String)],
    algo: &str,
    secondary: &str,
) -> bool {
    let mut others = all.iter().filter(|s| s.name != cand.name).peekable();
    others.peek().is_some()
        && others.all(|s| cand.prio > s.prio)
        && comp_sel == Some(algo)
        && recomp.iter().any(|(p, a)| *p == RECOMP_PRIO && a == secondary)
}

#[derive(Debug, PartialEq, Default, Clone, Copy)]
struct MmStat {
    orig: u64,
    compr: u64,
    mem_used: u64,
    mem_used_max: u64,
    same_pages: u64,
    huge_pages: u64,
}

/// mm_stat: orig_data_size compr_data_size mem_used_total mem_limit mem_used_max
/// same_pages pages_compacted huge_pages [huge_pages_since]. Older kernels have fewer columns.
fn parse_mm_stat(s: &str) -> Option<MmStat> {
    let v: Vec<u64> = s.split_whitespace().map(|x| x.parse().ok()).collect::<Option<_>>()?;
    if v.len() < 3 {
        return None;
    }
    let g = |i: usize| v.get(i).copied().unwrap_or(0);
    Some(MmStat {
        orig: g(0),
        compr: g(1),
        mem_used: g(2),
        mem_used_max: g(4),
        same_pages: g(5),
        huge_pages: g(7),
    })
}

/// `Key:   1234 kB` from /proc/meminfo, in bytes.
fn meminfo_bytes(s: &str, key: &str) -> Option<u64> {
    s.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|r| r.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

/// Effective uid from /proc/self/status (`Uid: real effective saved fs`).
fn parse_euid(status: &str) -> Option<u32> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|r| r.split_whitespace().nth(1)?.parse().ok())
}

/// debugfs block_state: `index  actime  flags`; the flags column has `r` for recompressed slots.
fn count_recompressed(block_state: &str) -> u64 {
    block_state
        .lines()
        .filter(|l| l.split_whitespace().nth(2).is_some_and(|f| f.contains('r')))
        .count() as u64
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Size {
    Pct(u64),
    Bytes(u64),
}

/// `50%`, `8G`, `512M`, `1048576`.
fn parse_size(s: &str) -> Option<Size> {
    let s = s.trim();
    if let Some(p) = s.strip_suffix('%') {
        return p.parse().ok().filter(|p| (1..=200).contains(p)).map(Size::Pct);
    }
    let (num, mul) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1u64 << 10),
        'M' | 'm' => (&s[..s.len() - 1], 1 << 20),
        'G' | 'g' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.parse::<u64>().ok().and_then(|n| n.checked_mul(mul)).map(Size::Bytes)
}

fn resolve_size(sz: Size, memtotal: u64) -> Result<u64, String> {
    let b = match sz {
        Size::Pct(p) => memtotal.saturating_mul(p) / 100,
        Size::Bytes(b) => b,
    };
    let b = b / 4096 * 4096;
    if b < 64 << 20 {
        return Err(format!("zram ölçüsü çox kiçikdir ({}); ən az 64M", hb(b)));
    }
    if b > memtotal.saturating_mul(2) {
        return Err(format!("zram ölçüsü RAM-ın 2 mislindən böyük ola bilməz ({} > {})", hb(b), hb(memtotal * 2)));
    }
    Ok(b)
}

/// Algorithm names go into `key=value` sysfs strings, so allow only plain names.
fn valid_algo(s: &str) -> bool {
    !s.is_empty() && s.len() < 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ───────────────────────────── arguments ─────────────────────────────

#[derive(Debug, PartialEq)]
struct SetupOpts {
    size: Size,
    algo: String,
    recomp_algo: Option<String>,
    priority: Option<i32>,
}

#[derive(Debug, PartialEq)]
struct MaintainOpts {
    idle_secs: u64,
    loop_secs: Option<u64>,
    threshold: Option<u32>,
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Setup(SetupOpts),
    Maintain(MaintainOpts),
    Status,
    Remove { force: bool },
    Help,
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let sub = args.first().map(|s| s.as_str());
    let mut it = args.iter().skip(1);
    fn val<'a>(flag: &str, it: &mut impl Iterator<Item = &'a String>) -> Result<String, String> {
        it.next().cloned().ok_or(format!("{flag} üçün dəyər verilməyib"))
    }
    match sub {
        Some("setup") => {
            let mut o = SetupOpts {
                size: Size::Pct(DEFAULT_SIZE_PCT),
                algo: DEFAULT_ALGO.into(),
                recomp_algo: Some(DEFAULT_RECOMP_ALGO.into()),
                priority: None,
            };
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--size" => {
                        let v = val(a, &mut it)?;
                        o.size = parse_size(&v).ok_or(format!("yanlış ölçü: {v} (nümunə: 50%, 8G, 4096M)"))?;
                    }
                    "--algo" => o.algo = val(a, &mut it)?,
                    "--recomp-algo" => o.recomp_algo = Some(val(a, &mut it)?),
                    "--no-recomp" => o.recomp_algo = None,
                    "--priority" => {
                        let v = val(a, &mut it)?;
                        let p: i32 = v.parse().map_err(|_| format!("yanlış prioritet: {v}"))?;
                        if !(0..=PRIO_MAX).contains(&p) {
                            return Err(format!("prioritet 0..{PRIO_MAX} aralığında olmalıdır"));
                        }
                        o.priority = Some(p);
                    }
                    x => return Err(format!("naməlum parametr: {x}")),
                }
            }
            for a in std::iter::once(&o.algo).chain(o.recomp_algo.iter()) {
                if !valid_algo(a) {
                    return Err(format!("yanlış alqoritm adı: {a}"));
                }
            }
            Ok(Cmd::Setup(o))
        }
        Some("maintain") => {
            let mut o = MaintainOpts { idle_secs: DEFAULT_IDLE_SECS, loop_secs: None, threshold: None };
            while let Some(a) = it.next() {
                let v = match a.as_str() {
                    "--idle-secs" | "--loop" | "--threshold" => val(a, &mut it)?,
                    x => return Err(format!("naməlum parametr: {x}")),
                };
                let n: u64 = v.parse().map_err(|_| format!("{a}: rəqəm gözlənilir, '{v}' verilib"))?;
                match a.as_str() {
                    "--idle-secs" => {
                        if n > u32::MAX as u64 {
                            return Err("--idle-secs çox böyükdür".into());
                        }
                        o.idle_secs = n;
                    }
                    "--loop" => {
                        if n < 10 {
                            return Err("--loop ən az 10 saniyə olmalıdır".into());
                        }
                        o.loop_secs = Some(n);
                    }
                    _ => {
                        // The kernel rejects threshold >= huge_class_size (~3 KiB on 4K pages).
                        if n == 0 || n >= 4096 {
                            return Err("--threshold 1..4095 bayt aralığında olmalıdır".into());
                        }
                        o.threshold = Some(n as u32);
                    }
                }
            }
            Ok(Cmd::Maintain(o))
        }
        Some("status") if args.len() == 1 => Ok(Cmd::Status),
        Some("remove") => match args.get(1).map(|s| s.as_str()) {
            None => Ok(Cmd::Remove { force: false }),
            Some("--force") if args.len() == 2 => Ok(Cmd::Remove { force: true }),
            Some(x) => Err(format!("naməlum parametr: {x}")),
        },
        None | Some("help") | Some("--help") | Some("-h") => Ok(Cmd::Help),
        Some(x) => Err(format!("naməlum əmr: zram-tier {x}")),
    }
}

// ───────────────────────────── state file ─────────────────────────────

#[derive(Debug, PartialEq, Default, Clone)]
struct State {
    dev: u32,
    boot_id: String,
    /// Recorded before the disksize write, so an interrupted setup can still be verified.
    disksize: u64,
    algo: String,
    /// Empty when recompression is not configured.
    recomp_algo: String,
    prio: i32,
    complete: bool,
    /// Value of zswap `enabled` before we turned it off (None: untouched).
    zswap_orig: Option<String>,
    /// Fallback scheme (no ACTIME tracking): when we last wrote `all` to `idle`.
    idle_mark: u64,
    recomp_passes: u64,
    /// Approximate: sum of compr_data_size drops measured around each recompress write.
    recomp_saved: u64,
    last_pass: u64,
}

fn render_state(s: &State) -> String {
    let mut o = format!(
        "dev={}\nboot_id={}\ndisksize={}\nalgo={}\nrecomp_algo={}\nprio={}\ncomplete={}\n",
        s.dev, s.boot_id, s.disksize, s.algo, s.recomp_algo, s.prio, s.complete as u8
    );
    if let Some(z) = &s.zswap_orig {
        o += &format!("zswap_orig={z}\n");
    }
    o += &format!(
        "idle_mark={}\nrecomp_passes={}\nrecomp_saved={}\nlast_pass={}\n",
        s.idle_mark, s.recomp_passes, s.recomp_saved, s.last_pass
    );
    o
}

fn parse_state(text: &str) -> Option<State> {
    let mut s = State::default();
    let mut have_dev = false;
    for l in text.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        let n = || v.trim().parse::<u64>().unwrap_or(0);
        match k {
            "dev" => {
                s.dev = v.trim().parse().ok()?;
                have_dev = true;
            }
            "boot_id" => s.boot_id = v.trim().into(),
            "disksize" => s.disksize = n(),
            "algo" => s.algo = v.trim().into(),
            "recomp_algo" => s.recomp_algo = v.trim().into(),
            "prio" => s.prio = v.trim().parse().unwrap_or(0),
            "complete" => s.complete = v.trim() == "1",
            "zswap_orig" => s.zswap_orig = Some(v.trim().into()),
            "idle_mark" => s.idle_mark = n(),
            "recomp_passes" => s.recomp_passes = n(),
            "recomp_saved" => s.recomp_saved = n(),
            "last_pass" => s.last_pass = n(),
            _ => {}
        }
    }
    have_dev.then_some(s)
}

fn load_state(sys: &Sys) -> Result<Option<State>, String> {
    match read(&sys.state) {
        Ok(t) => parse_state(&t)
            .map(Some)
            .ok_or(format!("{} pozulub (dev= sətri yoxdur)", sys.state.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} oxuna bilmədi: {e}", sys.state.display())),
    }
}

fn save_state(sys: &Sys, s: &State) -> Result<(), String> {
    let tmp = sys.state.with_extension("tmp");
    fs::write(&tmp, render_state(s))
        .and_then(|_| fs::rename(&tmp, &sys.state))
        .map_err(|e| format!("vəziyyət faylı {} yazıla bilmədi: {e}", sys.state.display()))
}

fn boot_id(sys: &Sys) -> String {
    read(&sys.proc_.join("sys/kernel/random/boot_id")).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Is the recorded device still the one we created? Never act on it otherwise.
fn verify_owned(sys: &Sys, st: &State) -> Result<(), String> {
    if st.boot_id.is_empty() || st.boot_id != boot_id(sys) {
        return Err("vəziyyət faylı başqa yükləmədəndir (boot_id fərqlidir)".into());
    }
    let z = sys.zdir(st.dev);
    if !z.exists() {
        return Err(format!("zram{} artıq mövcud deyil", st.dev));
    }
    let initialised = read(&z.join("initstate")).map(|s| s.trim() == "1").unwrap_or(false);
    if !initialised && st.disksize != 0 {
        // We got as far as the disksize write; an uninitialised device under our index was
        // reset or removed and re-created by someone else since.
        return Err(format!(
            "zram{} işə salınmayıb, amma biz onu {} ölçüsü ilə qurmuşduq — başqa cihaz ola bilər",
            st.dev,
            hb(st.disksize)
        ));
    }
    if initialised {
        let ds: u64 = read(&z.join("disksize")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let algo = read(&z.join("comp_algorithm")).ok().and_then(|s| parse_algo_list(&s).1);
        if ds != st.disksize || algo.as_deref() != Some(st.algo.as_str()) {
            return Err(format!(
                "zram{} bizim yaratdığımız cihaza uyğun gəlmir (disksize {ds} / alqoritm {:?})",
                st.dev, algo
            ));
        }
    }
    Ok(())
}

fn swaps(sys: &Sys) -> Vec<Swap> {
    parse_swaps(&read(&sys.proc_.join("swaps")).unwrap_or_default())
}

fn swap_of<'a>(sys: &Sys, list: &'a [Swap], dev: u32) -> Option<&'a Swap> {
    let node = sys.node(dev).to_string_lossy().to_string();
    list.iter().find(|s| s.name == node)
}

/// A swapped-on zram device that looks like an orphaned tier of ours: (index, priority).
fn find_leftover(sys: &Sys, o: &SetupOpts, list: &[Swap]) -> Option<(u32, i32)> {
    // With --no-recomp, look for what a default setup would have left.
    let secondary = o.recomp_algo.as_deref().unwrap_or(DEFAULT_RECOMP_ALGO);
    list.iter().find_map(|s| {
        let i = sys.zram_index(&s.name)?;
        let z = sys.zdir(i);
        let comp = read(&z.join("comp_algorithm")).ok().and_then(|t| parse_algo_list(&t).1);
        let recomp = read(&z.join("recomp_algorithm")).map(|t| parse_recomp_list(&t)).unwrap_or_default();
        looks_like_leftover(s, list, comp.as_deref(), &recomp, &o.algo, secondary).then_some((i, s.prio))
    })
}

// ───────────────────────────── setup ─────────────────────────────

fn setup(sys: &Sys, o: &SetupOpts, run: Runner) -> Result<(), String> {
    let _lock = lock_wait(sys)?;
    if let Some(st) = load_state(sys)? {
        match verify_owned(sys, &st) {
            Ok(()) if st.complete && swap_of(sys, &swaps(sys), st.dev).is_some() => {
                println!(
                    "✓ zram tier artıq aktivdir: /dev/zram{} (prioritet {}). Heç nə dəyişdirilmədi.",
                    st.dev, st.prio
                );
                return Ok(());
            }
            Ok(()) => {
                println!("! Yarımçıq qalmış quraşdırma tapıldı (zram{}) — əvvəlcə təmizlənir.", st.dev);
                // Not forced: it may already hold swapped pages; the RAM check applies.
                teardown(sys, &st, run, false)?;
            }
            Err(why) => forget_stale(sys, &st, &why)?,
        }
    }
    if let Some((i, p)) = find_leftover(sys, o, &swaps(sys)) {
        return Err(format!(
            "/dev/zram{i} əvvəlki ultra-ram tier-inə oxşayır (əsas alqoritm {}, yenidən sıxma {}, ən yüksək swap prioriteti {p}), \
             amma vəziyyət faylı yoxdur. İkinci tier yaradılmadı, zram{i}-a toxunulmadı.\n  \
             Yoxlayın: cat /sys/block/zram{i}/comp_algorithm /sys/block/zram{i}/recomp_algorithm; swapon --show\n  \
             Bizimdirsə, əl ilə silin və setup-ı təkrarlayın: sudo swapoff /dev/zram{i} && echo {i} | sudo tee /sys/class/zram-control/hot_remove\n  \
             (swapoff-dan əvvəl əmin olun ki, onun içindəkilər üçün RAM çatır; zswap söndürülmüş qalıbsa, onu da əl ilə bərpa edin.)",
            o.algo,
            o.recomp_algo.as_deref().unwrap_or(DEFAULT_RECOMP_ALGO)
        ));
    }

    if !sys.zram_control.join("hot_add").exists() {
        // num_devices=0: load the module without letting it pre-create zram0.
        run("modprobe", &["zram".into(), "num_devices=0".into()])
            .map_err(|e| format!("zram modulu yüklənmədi: {e}"))?;
        if !sys.zram_control.join("hot_add").exists() {
            return Err(format!("{} yoxdur — kernel zram dəstəkləmir", sys.zram_control.display()));
        }
    }
    let meminfo = read(&sys.proc_.join("meminfo")).map_err(|e| format!("/proc/meminfo: {e}"))?;
    let memtotal = meminfo_bytes(&meminfo, "MemTotal").ok_or("MemTotal oxuna bilmədi")?;
    let disksize = resolve_size(o.size, memtotal)?;
    let existing = swaps(sys);
    let max_other = existing.iter().map(|s| s.prio).max();
    let prio = o.priority.unwrap_or_else(|| pick_priority(&existing));
    if let Some(m) = max_other {
        if prio <= m {
            println!("! Xəbərdarlıq: prioritet {prio} mövcud swap-ın ən yüksəyindən ({m}) böyük deyil — tier ilk növbədə istifadə olunmayacaq.");
        }
    }

    let hot_add = sys.zram_control.join("hot_add");
    let idx: u32 = read(&hot_add)
        .map_err(|e| format!("{} oxuna bilmədi (yeni zram cihazı yaradılmadı): {e}", hot_add.display()))?
        .trim()
        .parse()
        .map_err(|_| "hot_add gözlənilməz cavab qaytardı".to_string())?;
    println!("• Yeni cihaz yaradıldı: zram{idx}");

    let mut st = State { dev: idx, boot_id: boot_id(sys), prio, ..Default::default() };
    if let Err(e) = save_state(sys, &st) {
        // Without a record we could never safely remove it later — undo right away.
        let r = wr(&sys.zram_control.join("hot_remove"), &idx.to_string());
        return Err(format!("{e}; zram{idx} geri silindi: {}", r.map(|_| "bəli".into()).unwrap_or_else(|e| e)));
    }

    if let Err(e) = configure(sys, &mut st, o, disksize, run) {
        println!("✗ {e}");
        println!("• Geri qaytarılır…");
        return match teardown(sys, &st, run, true) {
            Ok(()) => Err("quraşdırma alınmadı, dəyişikliklər geri qaytarıldı".into()),
            Err(e2) => Err(format!("quraşdırma alınmadı, geri qaytarma da tam alınmadı: {e2}")),
        };
    }

    let recomp = if st.recomp_algo.is_empty() {
        "yoxdur".to_string()
    } else {
        format!("{} (boş qalan səhifələr üçün)", st.recomp_algo)
    };
    println!(
        "✓ zram tier aktivdir: /dev/zram{idx}  {}  əsas alqoritm {}  yenidən sıxma {}  swap prioriteti {}",
        hb(disksize),
        st.algo,
        recomp,
        prio
    );
    Ok(())
}

fn configure(sys: &Sys, st: &mut State, o: &SetupOpts, disksize: u64, run: Runner) -> Result<(), String> {
    let z = sys.zdir(st.dev);
    let comp = z.join("comp_algorithm");
    let avail = parse_algo_list(&read(&comp).map_err(|e| format!("{}: {e}", comp.display()))?).0;
    if !avail.iter().any(|a| *a == o.algo) {
        return Err(format!("kernel-in zram-ı '{}' alqoritmini dəstəkləmir (mövcud: {})", o.algo, avail.join(" ")));
    }
    st.algo = o.algo.clone();
    st.disksize = disksize;
    save_state(sys, st)?;
    wr(&comp, &o.algo)?;

    if let Some(r) = &o.recomp_algo {
        let rp = z.join("recomp_algorithm");
        if !rp.exists() {
            println!("! Kernel CONFIG_ZRAM_MULTI_COMP olmadan qurulub — yenidən sıxma (recompress) mümkün deyil; yalnız {} işləyəcək.", o.algo);
        } else if !avail.iter().any(|a| a == r) {
            println!("! Kernel-in zram-ı '{r}' dəstəkləmir — yenidən sıxma olmayacaq (mövcud: {}).", avail.join(" "));
        } else {
            match wr(&rp, &format!("algo={r} priority={RECOMP_PRIO}")) {
                Ok(()) => {
                    st.recomp_algo = r.clone();
                    save_state(sys, st)?;
                }
                Err(e) => println!("! {e} — yenidən sıxma olmayacaq."),
            }
        }
    }

    wr(&z.join("disksize"), &disksize.to_string())?;

    // udev creates the node asynchronously after hot_add.
    let node = sys.node(st.dev);
    let mut waited = 0;
    while !node.exists() {
        if waited >= 100 {
            return Err(format!("{} 10 saniyə ərzində yaranmadı (udev?)", node.display()));
        }
        thread::sleep(Duration::from_millis(100));
        waited += 1;
    }
    let n = node.to_string_lossy().to_string();
    run("mkswap", &[n.clone()])?;
    run("swapon", &["-p".into(), st.prio.to_string(), n])?;

    // zswap in front of zram compresses everything twice; turn it off while the tier exists.
    if let Ok(v) = read(&sys.zswap_enabled) {
        let v = v.trim().to_string();
        if v == "Y" || v == "1" {
            st.zswap_orig = Some(v);
            save_state(sys, st)?;
            match wr(&sys.zswap_enabled, "N") {
                Ok(()) => println!("• zswap söndürüldü (zram-ın qarşısında ikiqat sıxma olmasın); remove zamanı bərpa olunacaq."),
                Err(e) => {
                    println!("! zswap söndürülə bilmədi: {e}");
                    st.zswap_orig = None;
                }
            }
        }
    }
    st.complete = true;
    save_state(sys, st)
}

// ───────────────────────────── remove ─────────────────────────────

/// A state file that no longer matches reality: drop it without touching any device.
fn forget_stale(sys: &Sys, st: &State, why: &str) -> Result<(), String> {
    println!("! Köhnə vəziyyət faylı: {why}. Heç bir cihaza toxunulmur, fayl silinir.");
    // Only restore zswap within the same boot; after a reboot it is back at its boot default.
    if let (Some(z), true) = (&st.zswap_orig, st.boot_id == boot_id(sys)) {
        match wr(&sys.zswap_enabled, z) {
            Ok(()) => println!("• zswap əvvəlki vəziyyətinə ({z}) qaytarıldı."),
            Err(e) => println!("! zswap bərpa olunmadı: {e}"),
        }
    }
    fs::remove_file(&sys.state).map_err(|e| format!("{} silinmədi: {e}", sys.state.display()))
}

fn teardown(sys: &Sys, st: &State, run: Runner, force: bool) -> Result<(), String> {
    let mut st = st.clone();
    let list = swaps(sys);
    if let Some(me) = swap_of(sys, &list, st.dev) {
        if !force && me.used_kb > 0 {
            let meminfo = read(&sys.proc_.join("meminfo")).unwrap_or_default();
            let avail = meminfo_bytes(&meminfo, "MemAvailable").unwrap_or(0);
            let total = meminfo_bytes(&meminfo, "MemTotal").unwrap_or(0);
            let tier_mem = read(&sys.zdir(st.dev).join("mm_stat")).ok().and_then(|s| parse_mm_stat(&s)).map_or(0, |m| m.mem_used);
            let disk_free: u64 = list
                .iter()
                .filter(|s| s.name != me.name && sys.zram_index(&s.name).is_none())
                .map(|s| s.size_kb.saturating_sub(s.used_kb) * 1024)
                .sum();
            let need = me.used_kb * 1024;
            if !swapoff_fits(need, avail, tier_mem, disk_free, total) {
                return Err(format!(
                    "zram{} içində {} var, amma boş RAM ({}) + tier-in özünün tutduğu RAM ({}) + diskdəki swap-da boş yer ({}) \
                     RAM-ın 10%-ni ({}) ehtiyatda saxlamağa kifayət etmir. swapoff edilmədi, tier olduğu kimi qalır. \
                     Yaddaş boşaldıqdan sonra yenidən cəhd edin (və ya risk bilərək: zram-tier remove --force).",
                    st.dev,
                    hb(need),
                    hb(avail),
                    hb(tier_mem),
                    hb(disk_free),
                    hb(total / 10)
                ));
            }
        }
        let n = sys.node(st.dev).to_string_lossy().to_string();
        if let Err(e) = run("swapoff", &[n]) {
            return Err(format!(
                "swapoff alınmadı ({e}). Çox güman ki, səhifələri geri qaytarmağa RAM çatmır; \
                 tier olduğu kimi saxlanılır, sonra yenidən cəhd edin."
            ));
        }
        println!("• swapoff /dev/zram{}", st.dev);
    }

    if let Some(z) = st.zswap_orig.take() {
        match wr(&sys.zswap_enabled, &z) {
            Ok(()) => println!("• zswap əvvəlki vəziyyətinə ({z}) qaytarıldı."),
            Err(e) => println!("! zswap bərpa olunmadı: {e}"),
        }
        let _ = save_state(sys, &st); // best effort: avoids restoring twice on a retry
    }

    if sys.zdir(st.dev).exists() {
        if let Err(e) = wr(&sys.zram_control.join("hot_remove"), &st.dev.to_string()) {
            st.complete = false;
            save_state(sys, &st)?;
            return Err(format!("zram{} silinmədi: {e}. Vəziyyət faylı saxlanıldı — yenidən cəhd edin.", st.dev));
        }
        println!("• zram{} silindi (hot_remove)", st.dev);
    }
    fs::remove_file(&sys.state).map_err(|e| format!("{} silinmədi: {e}", sys.state.display()))
}

fn remove(sys: &Sys, force: bool, run: Runner) -> Result<(), String> {
    let _lock = lock_wait(sys)?;
    let Some(st) = load_state(sys)? else {
        println!("Aktiv zram tier yoxdur — ediləcək heç nə yoxdur.");
        return Ok(());
    };
    if let Err(why) = verify_owned(sys, &st) {
        return forget_stale(sys, &st, &why);
    }
    teardown(sys, &st, run, force)?;
    println!("✓ zram tier silindi.");
    Ok(())
}

// ───────────────────────────── maintain ─────────────────────────────

#[derive(Debug, PartialEq)]
enum Pass {
    /// Recompression ran: compressed bytes before / after.
    Recompressed { before: u64, after: u64, actime: bool },
    /// Fallback scheme: just marked everything idle, recompress on a later pass.
    Marked,
    /// Fallback scheme: marked recently, waiting until idle_secs have passed.
    Waiting { left: u64 },
    /// Kernel or setup has no secondary algorithm.
    NoRecomp,
    /// setup/remove (or another maintain) holds the lock: this pass is skipped.
    Busy,
}

/// maintain must never resurrect a state file that `remove` deleted meanwhile.
fn save_existing(sys: &Sys, st: &State) -> Result<(), String> {
    if !sys.state.exists() {
        return Err(format!("{} yox oldu (tier silinib?) — yenidən yaradılmadı", sys.state.display()));
    }
    save_state(sys, st)
}

/// Fallback (no ACTIME): recompress once the `all` mark is at least idle_secs old.
fn fallback_due(now: u64, idle_mark: u64, idle_secs: u64) -> bool {
    idle_mark != 0 && now.saturating_sub(idle_mark) >= idle_secs
}

fn maintain_pass(sys: &Sys, o: &MaintainOpts) -> Result<Pass, String> {
    let Some(_lock) = try_lock(sys)? else { return Ok(Pass::Busy) };
    let mut st = load_state(sys)?.ok_or("aktiv zram tier yoxdur (əvvəlcə: ultra-ram zram-tier setup)")?;
    verify_owned(sys, &st)?;
    if !st.complete {
        return Err("zram tier quraşdırması yarımçıqdır — setup-ı yenidən işə salın".into());
    }
    if st.recomp_algo.is_empty() {
        return Ok(Pass::NoRecomp);
    }
    let z = sys.zdir(st.dev);
    let idle = z.join("idle");
    let t = now();
    let mut actime = true;
    if o.idle_secs == 0 {
        wr(&idle, "all")?;
        st.idle_mark = t;
    } else {
        match sysfs_write(&idle, &o.idle_secs.to_string()) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(EINVAL) => {
                // No CONFIG_ZRAM_TRACK_ENTRY_ACTIME: two-pass scheme with `all`.
                actime = false;
                if !fallback_due(t, st.idle_mark, o.idle_secs) {
                    if st.idle_mark == 0 {
                        wr(&idle, "all")?;
                        st.idle_mark = t;
                        save_existing(sys, &st)?;
                        return Ok(Pass::Marked);
                    }
                    return Ok(Pass::Waiting { left: o.idle_secs - t.saturating_sub(st.idle_mark) });
                }
            }
            Err(e) => return Err(format!("'{}' → {} yazıla bilmədi: {e}", o.idle_secs, idle.display())),
        }
    }

    let mm = |p: &Path| read(p).ok().and_then(|s| parse_mm_stat(&s)).unwrap_or_default();
    let before = mm(&z.join("mm_stat")).compr;
    let mut cmd = format!("type=idle priority={RECOMP_PRIO}");
    if let Some(th) = o.threshold {
        cmd += &format!(" threshold={th}");
    }
    wr(&z.join("recompress"), &cmd)?;
    let after = mm(&z.join("mm_stat")).compr;

    if !actime {
        // Start the next idle window: whatever is untouched until next pass gets recompressed then.
        wr(&idle, "all")?;
        st.idle_mark = now();
    }
    st.recomp_passes += 1;
    st.recomp_saved += before.saturating_sub(after);
    st.last_pass = now();
    save_existing(sys, &st)?;
    Ok(Pass::Recompressed { before, after, actime })
}

fn report_pass(p: &Result<Pass, String>, o: &MaintainOpts) {
    match p {
        Ok(Pass::Recompressed { before, after, actime }) => println!(
            "✓ recompress ({}): sıxılmış ölçü {} → {} (qənaət ≈ {})",
            if *actime {
                format!("{} saniyədən çox toxunulmayan səhifələr", o.idle_secs)
            } else {
                "əvvəlki keçiddən bəri toxunulmayan səhifələr".into()
            },
            hb(*before),
            hb(*after),
            hb(before.saturating_sub(*after))
        ),
        Ok(Pass::Marked) => println!(
            "• Kernel-də ACTIME izləmə yoxdur: bütün səhifələr boş (idle) işarələndi, {} saniyə sonrakı keçiddə toxunulmayanlar yenidən sıxılacaq.",
            o.idle_secs
        ),
        Ok(Pass::Waiting { left }) => println!("• Növbəti yenidən sıxmaya {left} saniyə qalıb."),
        Ok(Pass::NoRecomp) => println!("• Bu tier-də yenidən sıxma alqoritmi yoxdur — ediləcək iş yoxdur."),
        Ok(Pass::Busy) => println!("• Başqa zram-tier əməliyyatı gedir (setup/remove) — bu keçid buraxıldı."),
        Err(e) => println!("✗ {e}"),
    }
}

fn maintain(sys: &Sys, o: &MaintainOpts) -> i32 {
    let Some(secs) = o.loop_secs else {
        let r = maintain_pass(sys, o);
        report_pass(&r, o);
        return if r.is_ok() { 0 } else { 1 };
    };
    // Service mode: keep going on transient errors; stop (exit 1) only when the tier itself is
    // gone. The unit's ExecStopPost runs `remove` only after a clean stop: systemd counts death
    // by its own SIGTERM as SERVICE_RESULT=success, an exit 1, crash or OOM kill is not.
    loop {
        let r = maintain_pass(sys, o);
        report_pass(&r, o);
        if r.is_err() && !matches!(load_state(sys), Ok(Some(_))) {
            return 1;
        }
        thread::sleep(Duration::from_secs(secs));
    }
}

// ───────────────────────────── status ─────────────────────────────

fn status(sys: &Sys) -> i32 {
    let zswap = read(&sys.zswap_enabled).map(|s| s.trim().to_string()).unwrap_or_else(|_| "?".into());
    let st = match load_state(sys) {
        Ok(Some(st)) => st,
        Ok(None) => {
            println!("zram tier aktiv deyil.  (zswap: {zswap})");
            println!("Qurmaq üçün (root): sudo ultra-ram zram-tier setup");
            return 0;
        }
        Err(e) => {
            println!("✗ {e}");
            return 1;
        }
    };
    if let Err(why) = verify_owned(sys, &st) {
        println!("! Vəziyyət faylı var, amma etibarsızdır: {why}");
        return 1;
    }
    let z = sys.zdir(st.dev);
    let list = swaps(sys);
    let comp = read(&z.join("comp_algorithm")).ok().and_then(|s| parse_algo_list(&s).1).unwrap_or("?".into());
    let recomp: Vec<String> = read(&z.join("recomp_algorithm"))
        .map(|s| parse_recomp_list(&s).into_iter().map(|(p, a)| format!("#{p} {a}")).collect())
        .unwrap_or_default();
    println!("═══ ZRAM TIER ═══");
    println!(
        "Cihaz /dev/zram{}  ölçü {}  vəziyyət {}",
        st.dev,
        hb(st.disksize),
        if st.complete { "aktiv" } else { "yarımçıq" }
    );
    println!(
        "Alqoritm: əsas {comp}  |  yenidən sıxma {}",
        if recomp.is_empty() { "yoxdur".into() } else { recomp.join(", ") }
    );
    println!("zswap: {zswap}");
    println!("\nSwap cihazları (yüksək prioritet əvvəl doldurulur):");
    let mut sorted = list.clone();
    sorted.sort_by_key(|s| -s.prio);
    for s in &sorted {
        let mine = swap_of(sys, &list, st.dev).is_some_and(|m| m.name == s.name);
        println!(
            "  {:<20} prioritet {:>6}  {:>8} / {:<8}{}",
            s.name,
            s.prio,
            hb(s.used_kb * 1024),
            hb(s.size_kb * 1024),
            if mine { "  ← tier" } else { "" }
        );
    }
    if let Some(m) = read(&z.join("mm_stat")).ok().and_then(|s| parse_mm_stat(&s)) {
        let ratio = if m.compr > 0 { m.orig as f64 / m.compr as f64 } else { 0.0 };
        println!("\nmm_stat:");
        println!("  orijinal {}  → sıxılmış {}  (x{ratio:.2})", hb(m.orig), hb(m.compr));
        println!(
            "  real RAM istifadəsi {}  (pik {})  eyni-dolu səhifələr {}  sıxılmayan (huge) {}",
            hb(m.mem_used),
            hb(m.mem_used_max),
            m.same_pages,
            m.huge_pages
        );
    }
    println!("\nYenidən sıxma (recompress):");
    if st.recomp_passes == 0 {
        println!("  hələ işə düşməyib");
    } else {
        println!(
            "  {} keçid, sıxılmış ölçüdə təxmini qənaət {}, son keçid {} saniyə əvvəl",
            st.recomp_passes,
            hb(st.recomp_saved),
            now().saturating_sub(st.last_pass)
        );
    }
    // Exact per-slot flags exist only in debugfs (root + CONFIG_ZRAM_MEMORY_TRACKING).
    match read(&sys.debugfs_zram.join(format!("zram{}/block_state", st.dev))) {
        Ok(b) => println!("  yenidən sıxılmış səhifələr hazırda: {} ({})", count_recompressed(&b), hb(count_recompressed(&b) * 4096)),
        Err(_) => println!("  (dəqiq say üçün root + CONFIG_ZRAM_MEMORY_TRACKING lazımdır: debugfs block_state)"),
    }
    0
}

// ───────────────────────────── entry point ─────────────────────────────

fn help() {
    println!(
        "ultra-ram zram-tier — istəyə bağlı zram səviyyəsi (lz4 + boş qalanlar üçün zstd)

  setup               [root] Yeni zram cihazı yarat: lz4, ən yüksək swap prioriteti
      --size S        Ölçü: 50% (standart), 8G, 4096M
      --algo A        Əsas alqoritm (standart lz4)
      --recomp-algo A Yenidən sıxma alqoritmi (standart zstd); --no-recomp söndürür
      --priority N    Swap prioriteti (standart: mövcudların ən yüksəyi + 10)
  maintain            [root] Bir keçid: uzun müddət toxunulmayan səhifələri zstd ilə yenidən sıx
      --idle-secs N   Neçə saniyə toxunulmayan səhifələr (standart 1800; 0 = hamısı, dərhal)
      --loop SECS     Dayanmadan hər SECS saniyədən bir təkrarla (xidmət rejimi)
      --threshold B   Yalnız B baytdan böyük sıxılmış səhifələr
  status              Tier cihazı, alqoritmlər, mm_stat, yenidən sıxma statistikası
  remove              [root] swapoff, cihazı sil, zswap-ı bərpa et
      --force         RAM yoxlamasını keç. Kernel swapoff-u RAM çatmadığı üçün rədd etmir:
                      səhifələr RAM-a çəkilməyə davam edir, sistem ağır yaddaş təzyiqinə düşə bilər

Mövcud swap cihazlarına (məs. zram0) heç vaxt toxunulmur."
    );
}

fn is_root(sys: &Sys) -> bool {
    read(&sys.proc_.join("self/status")).ok().and_then(|s| parse_euid(&s)) == Some(0)
}

/// `ultra-ram zram-tier ...`; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let cmd = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ {e}\n");
            help();
            return 2;
        }
    };
    let sys = Sys::real();
    let needs_root = matches!(cmd, Cmd::Setup(_) | Cmd::Maintain(_) | Cmd::Remove { .. });
    if needs_root && !is_root(&sys) {
        println!("✗ Bu əmr root tələb edir: sudo ultra-ram zram-tier {}", args.join(" "));
        return 1;
    }
    let done = |r: Result<(), String>| match r {
        Ok(()) => 0,
        Err(e) => {
            println!("✗ {e}");
            1
        }
    };
    match cmd {
        Cmd::Setup(o) => done(setup(&sys, &o, &run_cmd)),
        Cmd::Maintain(o) => maintain(&sys, &o),
        Cmd::Status => status(&sys),
        Cmd::Remove { force } => done(remove(&sys, force, &run_cmd)),
        Cmd::Help => {
            help();
            0
        }
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn algo_list() {
        let (all, sel) = parse_algo_list("lzo-rle lzo lz4 [zstd]\n");
        assert_eq!(all, s(&["lzo-rle", "lzo", "lz4", "zstd"]));
        assert_eq!(sel.as_deref(), Some("zstd"));
        let (all, sel) = parse_algo_list("[lzo-rle] lzo lz4 lz4hc 842 zstd");
        assert_eq!(all.len(), 6);
        assert_eq!(sel.as_deref(), Some("lzo-rle"));
        assert_eq!(parse_algo_list("lzo lz4").1, None);
        assert_eq!(parse_algo_list("").0.len(), 0);
    }

    #[test]
    fn recomp_list() {
        let r = parse_recomp_list("#1: lzo lzo-rle lz4 lz4hc [zstd]\n#2: lzo lzo-rle lz4 [lz4hc] zstd\n");
        assert_eq!(r, vec![(1, "zstd".to_string()), (2, "lz4hc".to_string())]);
        assert!(parse_recomp_list("").is_empty());
    }

    const SWAPS: &str = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority
/dev/zram0                              partition\t8388604\t\t1024\t\t100
/swap\\040file                           file\t\t2097148\t\t0\t\t-2
";

    #[test]
    fn swaps_and_priority() {
        let v = parse_swaps(SWAPS);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0], Swap { name: "/dev/zram0".into(), size_kb: 8388604, used_kb: 1024, prio: 100 });
        assert_eq!(v[1].prio, -2);
        assert_eq!(pick_priority(&v), 110);
        assert_eq!(pick_priority(&[]), PRIO_NO_SWAP);
        assert_eq!(pick_priority(&parse_swaps("Filename Type Size Used Priority\n/dev/sda2 partition 10 0 -2\n")), 8);
        let top = parse_swaps("h\n/dev/zram0 partition 10 0 32765\n");
        assert_eq!(pick_priority(&top), PRIO_MAX);
        assert!(parse_swaps("Filename Type Size Used Priority\n").is_empty());
    }

    #[test]
    fn mm_stat() {
        let m = parse_mm_stat("4194304000 1677721600 1700000000        0 1800000000    12345        7      42      100\n").unwrap();
        assert_eq!(m.orig, 4194304000);
        assert_eq!(m.compr, 1677721600);
        assert_eq!(m.mem_used, 1700000000);
        assert_eq!(m.mem_used_max, 1800000000);
        assert_eq!(m.same_pages, 12345);
        assert_eq!(m.huge_pages, 42);
        let old = parse_mm_stat("100 50 60").unwrap();
        assert_eq!((old.orig, old.compr, old.mem_used, old.huge_pages), (100, 50, 60, 0));
        assert!(parse_mm_stat("100 50").is_none());
        assert!(parse_mm_stat("x y z").is_none());
    }

    #[test]
    fn misc_parsers() {
        assert_eq!(meminfo_bytes("MemTotal:       16303428 kB\nMemFree: 1 kB\n", "MemTotal"), Some(16303428 * 1024));
        assert_eq!(meminfo_bytes("MemTotalX: 5 kB\n", "MemTotal"), None);
        assert_eq!(parse_euid("Name:\tx\nUid:\t1000\t0\t0\t0\n"), Some(0));
        assert_eq!(parse_euid("Uid:\t1000\t1000\t1000\t1000\n"), Some(1000));
        assert_eq!(count_recompressed("  300    75.033841 .wh...\n  303    62.801919 ....r.\n  304   1.0 ..hi.n\n"), 1);
        assert_eq!(parse_size("50%"), Some(Size::Pct(50)));
        assert_eq!(parse_size("8G"), Some(Size::Bytes(8 << 30)));
        assert_eq!(parse_size("512m"), Some(Size::Bytes(512 << 20)));
        assert_eq!(parse_size("1048576"), Some(Size::Bytes(1048576)));
        assert_eq!(parse_size("0%"), None);
        assert_eq!(parse_size("abc"), None);
        let gb16 = 16u64 << 30;
        assert_eq!(resolve_size(Size::Pct(50), gb16), Ok(8 << 30));
        assert!(resolve_size(Size::Bytes(1 << 20), gb16).is_err());
        assert!(resolve_size(Size::Bytes(40 << 30), gb16).is_err());
        assert!(valid_algo("lzo-rle") && valid_algo("zstd"));
        assert!(!valid_algo("zstd priority=2") && !valid_algo(""));
        assert!(!fallback_due(1000, 0, 10));
        assert!(!fallback_due(1005, 1000, 10));
        assert!(fallback_due(1010, 1000, 10));
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(&s(&[])), Ok(Cmd::Help));
        assert_eq!(
            parse_args(&s(&["setup"])),
            Ok(Cmd::Setup(SetupOpts { size: Size::Pct(50), algo: "lz4".into(), recomp_algo: Some("zstd".into()), priority: None }))
        );
        assert_eq!(
            parse_args(&s(&["setup", "--size", "4G", "--no-recomp", "--priority", "200"])),
            Ok(Cmd::Setup(SetupOpts { size: Size::Bytes(4 << 30), algo: "lz4".into(), recomp_algo: None, priority: Some(200) }))
        );
        assert!(parse_args(&s(&["setup", "--priority", "40000"])).is_err());
        assert!(parse_args(&s(&["setup", "--algo", "lz4 priority=2"])).is_err());
        assert!(parse_args(&s(&["setup", "--size"])).is_err());
        assert_eq!(
            parse_args(&s(&["maintain"])),
            Ok(Cmd::Maintain(MaintainOpts { idle_secs: 1800, loop_secs: None, threshold: None }))
        );
        assert_eq!(
            parse_args(&s(&["maintain", "--idle-secs", "0", "--loop", "300", "--threshold", "2000"])),
            Ok(Cmd::Maintain(MaintainOpts { idle_secs: 0, loop_secs: Some(300), threshold: Some(2000) }))
        );
        assert!(parse_args(&s(&["maintain", "--loop", "1"])).is_err());
        assert!(parse_args(&s(&["maintain", "--threshold", "5000"])).is_err());
        assert!(parse_args(&s(&["maintain", "--idle-secs", "x"])).is_err());
        assert_eq!(parse_args(&s(&["status"])), Ok(Cmd::Status));
        assert_eq!(parse_args(&s(&["remove"])), Ok(Cmd::Remove { force: false }));
        assert_eq!(parse_args(&s(&["remove", "--force"])), Ok(Cmd::Remove { force: true }));
        assert!(parse_args(&s(&["remove", "--yes"])).is_err());
        assert!(parse_args(&s(&["bogus"])).is_err());
    }

    #[test]
    fn state_roundtrip() {
        let st = State {
            dev: 3,
            boot_id: "abc".into(),
            disksize: 8 << 30,
            algo: "lz4".into(),
            recomp_algo: "zstd".into(),
            prio: 110,
            complete: true,
            zswap_orig: Some("Y".into()),
            idle_mark: 5,
            recomp_passes: 2,
            recomp_saved: 1000,
            last_pass: 9,
        };
        assert_eq!(parse_state(&render_state(&st)), Some(st.clone()));
        let st2 = State { zswap_orig: None, ..st };
        assert_eq!(parse_state(&render_state(&st2)), Some(st2));
        assert_eq!(parse_state("garbage\n"), None);
    }

    // ── end-to-end against a fake sysfs tree ──

    struct Fake {
        root: PathBuf,
        sys: Sys,
    }

    impl Fake {
        fn new(name: &str) -> Fake {
            let root = std::env::temp_dir().join(format!("ultra-ram-zt-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            let sys = Sys::under(&root);
            for d in ["sys/block/zram0", "sys/class/zram-control", "proc/sys/kernel/random", "sys/module/zswap/parameters", "dev", "run"] {
                fs::create_dir_all(root.join(d)).unwrap();
            }
            let f = Fake { root, sys };
            f.put("proc/sys/kernel/random/boot_id", "boot-1\n");
            f.put("proc/meminfo", "MemTotal:       16777216 kB\nMemAvailable:    8388608 kB\n");
            f.put("proc/swaps", "Filename Type Size Used Priority\n/dev/zram0 partition 8388604 0 100\n");
            f.put("sys/module/zswap/parameters/enabled", "Y\n");
            f.put("sys/class/zram-control/hot_add", "1\n");
            f.put("sys/class/zram-control/hot_remove", "");
            // The device that hot_add will "create".
            for (k, v) in [
                ("comp_algorithm", "lzo-rle lzo lz4 [zstd]\n"),
                ("recomp_algorithm", ""),
                ("disksize", "0\n"),
                ("initstate", "0\n"),
                ("idle", ""),
                ("recompress", ""),
                ("mm_stat", "4096000 2048000 2100000 0 2100000 0 0 0 0\n"),
            ] {
                f.put(&format!("sys/block/zram1/{k}"), v);
            }
            f.put("dev/zram1", "");
            f
        }
        fn put(&self, rel: &str, v: &str) {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, v).unwrap();
        }
        fn get(&self, rel: &str) -> String {
            fs::read_to_string(self.root.join(rel)).unwrap()
        }
        fn node(&self) -> String {
            self.sys.node(1).to_string_lossy().to_string()
        }
        /// After the writes a real kernel would have applied: initialized, algorithm selected.
        fn kernel_init(&self) {
            self.put("sys/block/zram1/initstate", "1\n");
            self.put("sys/block/zram1/comp_algorithm", "lzo-rle lzo [lz4] zstd\n");
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn setup_maintain_remove_cycle() {
        let f = Fake::new("cycle");
        let calls = RefCell::new(Vec::<String>::new());
        let run = |p: &str, a: &[String]| -> Result<(), String> {
            calls.borrow_mut().push(format!("{p} {}", a.join(" ")));
            if p == "swapon" {
                let mut sw = f.get("proc/swaps");
                sw += &format!("{} partition 8388604 0 {}\n", a[2], a[1]);
                f.put("proc/swaps", &sw);
            }
            Ok(())
        };
        let o = match parse_args(&s(&["setup"])).unwrap() {
            Cmd::Setup(o) => o,
            _ => unreachable!(),
        };
        setup(&f.sys, &o, &run).unwrap();
        assert_eq!(f.get("sys/block/zram1/comp_algorithm"), "lz4");
        assert_eq!(f.get("sys/block/zram1/recomp_algorithm"), "algo=zstd priority=1");
        assert_eq!(f.get("sys/block/zram1/disksize"), (8u64 << 30).to_string());
        assert_eq!(f.get("sys/module/zswap/parameters/enabled"), "N");
        assert_eq!(*calls.borrow(), vec![format!("mkswap {}", f.node()), format!("swapon -p 110 {}", f.node())]);
        let st = load_state(&f.sys).unwrap().unwrap();
        assert!(st.complete && st.dev == 1 && st.zswap_orig.as_deref() == Some("Y"));
        // zram0 (not ours) was never written.
        assert!(!f.root.join("sys/block/zram0/comp_algorithm").exists());

        f.kernel_init();
        // Idempotent: no new calls.
        setup(&f.sys, &o, &run).unwrap();
        assert_eq!(calls.borrow().len(), 2);

        // maintain with ACTIME available (fake write succeeds).
        let mo = MaintainOpts { idle_secs: 1800, loop_secs: None, threshold: Some(2000) };
        let p = maintain_pass(&f.sys, &mo).unwrap();
        assert!(matches!(p, Pass::Recompressed { actime: true, .. }));
        assert_eq!(f.get("sys/block/zram1/idle"), "1800");
        assert_eq!(f.get("sys/block/zram1/recompress"), "type=idle priority=1 threshold=2000");
        assert_eq!(load_state(&f.sys).unwrap().unwrap().recomp_passes, 1);

        // Fingerprint mismatch → maintain refuses to touch the device.
        f.put("sys/block/zram1/disksize", "123\n");
        assert!(maintain_pass(&f.sys, &mo).is_err());
        f.put("sys/block/zram1/disksize", &(8u64 << 30).to_string());

        remove(&f.sys, false, &run).unwrap();
        assert_eq!(calls.borrow().last().unwrap(), &format!("swapoff {}", f.node()));
        assert_eq!(f.get("sys/class/zram-control/hot_remove"), "1");
        assert_eq!(f.get("sys/module/zswap/parameters/enabled"), "Y");
        assert!(load_state(&f.sys).unwrap().is_none());
        // Nothing left to do.
        remove(&f.sys, false, &run).unwrap();
    }

    #[test]
    fn setup_without_multi_comp_and_failure_rollback() {
        let f = Fake::new("nomc");
        fs::remove_file(f.root.join("sys/block/zram1/recomp_algorithm")).unwrap();
        let ok = |_: &str, _: &[String]| -> Result<(), String> { Ok(()) };
        let o = SetupOpts { size: Size::Pct(25), algo: "lz4".into(), recomp_algo: Some("zstd".into()), priority: None };
        setup(&f.sys, &o, &ok).unwrap();
        let st = load_state(&f.sys).unwrap().unwrap();
        assert!(st.complete && st.recomp_algo.is_empty());
        f.kernel_init();
        assert_eq!(maintain_pass(&f.sys, &MaintainOpts { idle_secs: 60, loop_secs: None, threshold: None }), Ok(Pass::NoRecomp));

        // A failing mkswap rolls the new device back.
        let g = Fake::new("fail");
        let bad = |p: &str, _: &[String]| -> Result<(), String> {
            if p == "mkswap" {
                Err("boom".into())
            } else {
                Ok(())
            }
        };
        assert!(setup(&g.sys, &o, &bad).is_err());
        assert_eq!(g.get("sys/class/zram-control/hot_remove"), "1");
        assert!(load_state(&g.sys).unwrap().is_none());
        // zswap untouched because we never got that far.
        assert_eq!(g.get("sys/module/zswap/parameters/enabled"), "Y\n");
    }

    #[test]
    fn stale_state_is_dropped_without_touching_device() {
        let f = Fake::new("stale");
        let st = State { dev: 1, boot_id: "other-boot".into(), zswap_orig: Some("Y".into()), ..Default::default() };
        save_state(&f.sys, &st).unwrap();
        let never = |p: &str, _: &[String]| -> Result<(), String> { panic!("must not run {p}") };
        remove(&f.sys, false, &never).unwrap();
        assert!(load_state(&f.sys).unwrap().is_none());
        assert_eq!(f.get("sys/class/zram-control/hot_remove"), "");
        // Different boot: zswap left as the current boot set it.
        assert_eq!(f.get("sys/module/zswap/parameters/enabled"), "Y\n");
    }

    #[test]
    fn swapoff_refused_when_ram_is_short() {
        let f = Fake::new("short");
        let ok = |_: &str, _: &[String]| -> Result<(), String> { Ok(()) };
        let o = SetupOpts { size: Size::Pct(50), algo: "lz4".into(), recomp_algo: None, priority: None };
        setup(&f.sys, &o, &ok).unwrap();
        f.kernel_init();
        f.put(
            "proc/swaps",
            &format!("h\n/dev/zram0 partition 1000 1000 100\n{} partition 8388604 8000000 110\n", f.node()),
        );
        f.put("proc/meminfo", "MemTotal: 16777216 kB\nMemAvailable: 1000 kB\n");
        let never = |p: &str, _: &[String]| -> Result<(), String> { panic!("must not run {p}") };
        assert!(remove(&f.sys, false, &never).is_err());
        assert!(load_state(&f.sys).unwrap().is_some());
        assert_eq!(f.get("sys/module/zswap/parameters/enabled"), "N");
    }

    #[test]
    fn ram_check_math() {
        let g = 1u64 << 30;
        // 16G RAM keeps 1.6G spare: 4G back needs 5.6G from RAM + tier RAM + disk swap.
        assert!(!swapoff_fits(4 * g, 3 * g, g, 0, 16 * g));
        assert!(swapoff_fits(4 * g, 5 * g, g, 0, 16 * g));
        assert!(swapoff_fits(4 * g, 3 * g, g, 2 * g, 16 * g));
        assert!(swapoff_fits(0, 0, 0, 0, 0));
        assert!(!swapoff_fits(u64::MAX, 8 * g, 0, 0, 16 * g), "no overflow");
        assert!(swapoff_fits(g, u64::MAX, u64::MAX, u64::MAX, u64::MAX), "no overflow");
        let f = Fake::new("idx");
        assert_eq!(f.sys.zram_index(&f.sys.node(12).to_string_lossy()), Some(12));
        assert_eq!(f.sys.zram_index(&format!("{}/zram", f.sys.dev.display())), None);
        assert_eq!(f.sys.zram_index(&format!("{}/zram+1", f.sys.dev.display())), None);
        assert_eq!(f.sys.zram_index("/swapfile"), None);
    }

    /// Our tier at `used` KiB plus the given extra /proc/swaps lines; returns remove()'s result.
    /// `{zram0}` in `extra` stands for the fake /dev/zram0.
    fn remove_with(name: &str, avail_kb: u64, used_kb: u64, extra: &str) -> (Result<(), String>, Vec<String>) {
        let f = Fake::new(name);
        let extra = extra.replace("{zram0}", &f.sys.node(0).to_string_lossy());
        let ok = |_: &str, _: &[String]| -> Result<(), String> { Ok(()) };
        let o = SetupOpts { size: Size::Pct(50), algo: "lz4".into(), recomp_algo: None, priority: None };
        setup(&f.sys, &o, &ok).unwrap();
        f.kernel_init();
        f.put("proc/swaps", &format!("h\n{extra}{} partition 8388604 {used_kb} 110\n", f.node()));
        f.put("proc/meminfo", &format!("MemTotal: 16777216 kB\nMemAvailable: {avail_kb} kB\n"));
        // The tier itself holds 1G of RAM.
        f.put("sys/block/zram1/mm_stat", &format!("0 0 {} 0 0 0 0 0\n", 1u64 << 30));
        let calls = RefCell::new(Vec::new());
        let rec = |p: &str, _: &[String]| -> Result<(), String> {
            calls.borrow_mut().push(p.to_string());
            Ok(())
        };
        let r = remove(&f.sys, false, &rec);
        let c = calls.borrow().clone();
        (r, c)
    }

    #[test]
    fn ram_check_ignores_other_zram_and_counts_disk_swap() {
        // 4G in the tier, 3G available + 1G tier RAM < 4G + 1.6G spare.
        let (r, c) = remove_with("rc1", 3 << 20, 4 << 20, "");
        assert!(r.is_err() && c.is_empty());
        // 8G free on the distro zram0 does not help: that would cost RAM again.
        let (r, c) = remove_with("rc2", 3 << 20, 4 << 20, "{zram0} partition 8388604 0 100\n");
        assert!(r.is_err() && c.is_empty(), "{r:?}");
        // 8G free on a disk swap file does.
        let (r, c) = remove_with("rc3", 3 << 20, 4 << 20, "/swap/file file 8388604 0 -2\n");
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(c, vec!["swapoff".to_string()]);
        // Enough RAM on its own.
        let (r, _) = remove_with("rc4", 5 << 20, 4 << 20, "");
        assert!(r.is_ok(), "{r:?}");
    }

    #[test]
    fn leftover_detection() {
        let sw = |name: &str, prio: i32| Swap { name: name.into(), size_kb: 1, used_kb: 0, prio };
        let recomp = vec![(1, "zstd".to_string())];
        let tier = sw("/dev/zram1", 110);
        let all = vec![sw("/dev/zram0", 100), tier.clone()];
        assert!(looks_like_leftover(&tier, &all, Some("lz4"), &recomp, "lz4", "zstd"));
        // Not above everything else.
        let all2 = vec![sw("/dev/zram0", 110), tier.clone()];
        assert!(!looks_like_leftover(&tier, &all2, Some("lz4"), &recomp, "lz4", "zstd"));
        // The only swap device: a distro zram0 with lz4.
        assert!(!looks_like_leftover(&tier, &[tier.clone()], Some("lz4"), &recomp, "lz4", "zstd"));
        // Other primary, no or another secondary.
        assert!(!looks_like_leftover(&tier, &all, Some("zstd"), &recomp, "lz4", "zstd"));
        assert!(!looks_like_leftover(&tier, &all, Some("lz4"), &[], "lz4", "zstd"));
        assert!(!looks_like_leftover(&tier, &all, Some("lz4"), &[(2, "zstd".into())], "lz4", "zstd"));
        assert!(!looks_like_leftover(&tier, &all, None, &recomp, "lz4", "zstd"));

        // End to end: an orphaned tier zram5 above zram0, no state file → setup refuses, no hot_add.
        let f = Fake::new("leftover");
        let z5 = f.sys.node(5).to_string_lossy().to_string();
        f.put("proc/swaps", &format!("h\n/dev/zram0 partition 8388604 0 100\n{z5} partition 8388604 10 110\n"));
        f.put("sys/block/zram5/comp_algorithm", "lzo [lz4] zstd\n");
        f.put("sys/block/zram5/recomp_algorithm", "#1: lzo lz4 [zstd]\n");
        let never = |p: &str, _: &[String]| -> Result<(), String> { panic!("must not run {p}") };
        let o = SetupOpts { size: Size::Pct(50), algo: "lz4".into(), recomp_algo: Some("zstd".into()), priority: None };
        let e = setup(&f.sys, &o, &never).unwrap_err();
        assert!(e.contains("zram5"), "{e}");
        assert!(load_state(&f.sys).unwrap().is_none());
        assert_eq!(f.get("sys/block/zram1/disksize"), "0\n", "no new device configured");
        // Same device as the distro's (zstd primary): not ours, setup goes ahead.
        f.put("sys/block/zram5/comp_algorithm", "lzo lz4 [zstd]\n");
        let ok = |_: &str, _: &[String]| -> Result<(), String> { Ok(()) };
        setup(&f.sys, &o, &ok).unwrap();
        assert!(load_state(&f.sys).unwrap().unwrap().complete);
    }

    #[test]
    fn lock_and_vanished_state() {
        let f = Fake::new("lock");
        let ok = |_: &str, _: &[String]| -> Result<(), String> { Ok(()) };
        let o = SetupOpts { size: Size::Pct(25), algo: "lz4".into(), recomp_algo: Some("zstd".into()), priority: None };
        setup(&f.sys, &o, &ok).unwrap();
        f.kernel_init();
        let mo = MaintainOpts { idle_secs: 60, loop_secs: None, threshold: None };
        {
            let held = try_lock(&f.sys).unwrap().unwrap();
            assert!(try_lock(&f.sys).unwrap().is_none());
            assert_eq!(maintain_pass(&f.sys, &mo), Ok(Pass::Busy));
            assert_eq!(f.get("sys/block/zram1/recompress"), "", "nothing written while locked");
            drop(held);
        }
        assert!(matches!(maintain_pass(&f.sys, &mo), Ok(Pass::Recompressed { .. })));
        // A pass must not bring back a state file that vanished.
        let st = load_state(&f.sys).unwrap().unwrap();
        fs::remove_file(&f.sys.state).unwrap();
        assert!(save_existing(&f.sys, &st).is_err());
        assert!(!f.sys.state.exists());
    }

    #[test]
    fn verify_owned_rejects_reset_device_and_incomplete_cleanup_is_not_forced() {
        let f = Fake::new("owned");
        let st = State { dev: 1, boot_id: "boot-1".into(), disksize: 8 << 30, algo: "lz4".into(), ..Default::default() };
        // initstate 0 but we recorded a disksize: someone reset / re-created zram1.
        assert!(verify_owned(&f.sys, &st).unwrap_err().contains("işə salınmayıb"));
        // Before the disksize write (disksize 0) an uninitialised device is expected.
        assert!(verify_owned(&f.sys, &State { disksize: 0, ..st.clone() }).is_ok());

        // Incomplete setup that is already swapped on with 4G in it and too little RAM:
        // the cleanup obeys the RAM check instead of forcing swapoff.
        f.kernel_init();
        f.put("sys/block/zram1/disksize", &(8u64 << 30).to_string());
        save_state(&f.sys, &st).unwrap();
        f.put(
            "proc/swaps",
            &format!("h\n{} partition 8388604 0 100\n{} partition 8388604 4194304 110\n", f.sys.node(0).display(), f.node()),
        );
        f.put("proc/meminfo", "MemTotal: 16777216 kB\nMemAvailable: 1048576 kB\n");
        let never = |p: &str, _: &[String]| -> Result<(), String> { panic!("must not run {p}") };
        let o = SetupOpts { size: Size::Pct(50), algo: "lz4".into(), recomp_algo: None, priority: None };
        assert!(setup(&f.sys, &o, &never).is_err());
        assert!(load_state(&f.sys).unwrap().is_some());
    }
}
