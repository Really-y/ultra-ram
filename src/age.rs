//! Optional "age" helper: explicit MGLRU aging of every app cgroup, and a cold-memory map for
//! the rootless reclaim daemon.
//!
//! Problem: right after apps load, the multi-gen LRU has no age information, so the daemon's
//! first `memory.reclaim` bites take essentially random pages, hot ones included; later, when an
//! app's cold part is used up, reclaim reaches warm pages before the refault detector reacts.
//! This helper ages each app memcg every `--interval-secs` (default 60): the kernel walks the
//! app's page tables, moves every page it finds accessed into the youngest generations and clears
//! the accessed bits. A page nobody touches stays behind in an old generation; `memory.reclaim`
//! evicts from the oldest generation first. The helper then exports, per app, how much anonymous
//! memory sits in generations that have gone unaccessed for how long, so the daemon can cap a
//! bite at what is really cold. Nothing is evicted here: aging only sorts.
//!
//! Like prefetch and the zram tier it needs root and lives in its own subcommands
//! (`ultra-ram age ...`); the reclaim daemon itself stays a rootless user service.
//!
//! Kernel interface (checked against mm/vmscan.c of v6.1, v6.6, v6.12, v6.15, v6.18 and master,
//! include/linux/mmzone.h, include/linux/mm_inline.h and
//! Documentation/admin-guide/mm/multigen_lru.rst):
//! * Reading `/sys/kernel/debug/lru_gen` (`lru_gen_seq_show`, debugfs mode 0644 root; debugfs
//!   itself is 0700 root) lists every memcg as `memcg <id> <path>` (`%5hu` up to 6.18, `%llu` in
//!   master; the path comes from `cgroup_path()`, i.e. relative to the cgroup2 root and starting
//!   with `/`), then per memory node ` node <nid>` and one line per generation
//!   ` <seq> <age_ms> <anon_pages> <file_pages>`, oldest first. `age_ms` is the time since that
//!   generation was created (`jiffies - timestamps[gen]`, u32 milliseconds). `lru_gen_full`
//!   (0444) adds tier and mm-walk statistics lines and an `x` after counts below a type's min_seq.
//! * Writing `+ memcg_id node_id max_gen_nr [can_swap|swappiness [force_scan]]` runs
//!   `run_cmd` → `run_aging` → `try_to_inc_max_seq`. `max_gen_nr` larger than the lruvec's
//!   max_seq is -EINVAL (also an unknown memcg id or node); smaller is a silent no-op on ≤ 6.12
//!   and master (6.15–6.18 age anyway). The 5th field is `can_swap` up to 6.12 and `swappiness`
//!   (0–200 or `max`) since 6.15; we send `1 1`, which means "walk anon and file, force_scan" on
//!   both: anon VMAs are skipped only at swappiness 0, file VMAs only at `max`
//!   (`should_skip_vma`). force_scan (also the default) walks every mm of the memcg and every
//!   PMD, not only the ones its Bloom filter recorded (`get_next_mm`, `walk_pmd_range`).
//!   Since 6.15 `try_to_inc_max_seq` returning false gives -EEXIST: another walker (the kernel's
//!   own reclaim) is finishing that generation; we count it and do not retry.
//! * The walk only happens when `/sys/kernel/mm/lru_gen/enabled` has bit 0x0002 (`should_walk_mmu`:
//!   the CPU sets accessed bits and LRU_GEN_MM_WALK is on); otherwise aging just bumps the
//!   sequence number (`iterate_mm_list_nowalk`) and says nothing about idleness, so `run` refuses.
//!   The walk covers the processes of that exact memcg (`memcg->mm_list`), so child cgroups of an
//!   app are aged too.
//! * There are MIN_NR_GENS = 2 to MAX_NR_GENS = 4 generations. When aging would exceed 4,
//!   `inc_max_seq` calls `inc_min_seq`, which merges the oldest generation into the next one
//!   (pages promoted in the meantime go to their new generation): nothing is evicted, aging never
//!   fails for that reason. Exception: the type that the given swappiness excludes (anon at 0,
//!   file at `max`) is not moved and its oldest pages wrap into the youngest slot — one reason we
//!   never send 0 or `max`.
//! * Eviction (`evict_folios` → `scan_folios`) always takes the generation at min_seq of the type
//!   it scans and refuses when only MIN_NR_GENS are left (the two youngest are never evicted);
//!   `memory.reclaim swappiness=max` scans anon only. Before a page goes, `folio_check_references`
//!   still checks its accessed bits through rmap and keeps it if it was used since the last walk.
//! * New anon pages start in max_seq − 1, swapped-in ones in an older generation
//!   (`lru_gen_folio_seq`); a fresh lruvec starts with seqs 0..=3 all created at the same moment,
//!   without a walk (`lru_gen_init_lruvec`). `/sys/kernel/mm/lru_gen/min_ttl_ms` only makes kswapd
//!   OOM-kill when every generation is younger than it (`lru_gen_age_node`); it does not touch
//!   `memory.reclaim` and this helper does not use it.
//!
//! What "idle" means here: a walk at time t moves the pages it saw accessed into generation
//! max_seq and then creates max_seq + 1. So the pages of generation s were last seen accessed no
//! later than the creation of s + 1, and every later walk confirmed they stayed untouched (accesses
//! after the newest walk are caught by reclaim's own reference check). We therefore report for
//! generation s: idle_ms = age_ms of the first generation after s that a walk created (seq ≥ 4),
//! and 0 for the youngest generation and when no walk happened yet. With aging every T seconds the
//! oldest generation is idle ≥ 2T right after a tick (3T just before the next), the next one ≥ T.
//! So `cold_age_secs` of the daemon should be ≤ 2 × the interval.
//!
//! Files for the daemon (`AGE_DIR`, rewritten after every tick):
//! * path: `/run/ultra-ram-age/<name>`, `<name>` = the app cgroup's directory name with the same
//!   rules as prefetch's markers (`prefetch::mark_name`: characters outside `[A-Za-z0-9._@-]`
//!   and a leading `.` become `_`, names over 200 bytes get no file). When two app cgroups (of
//!   different users) map to the same name, neither gets a file. Directory 0755, files 0644,
//!   owned by root, each replaced atomically (`prefetch::write_file_in`); files of cgroups that
//!   no longer exist are deleted on the next tick.
//! * content: a header line `ultra-ram-age 1 <unix_seconds> <interval_secs>` (time the tick
//!   finished, aging interval), then one line per generation of the app cgroup and of all its
//!   descendant cgroups, on every node: `<idle_ms> <anon_bytes>`, oldest (largest idle_ms) first.
//! * use (main.rs `pass()`): a file younger than 3 intervals counts; the bytes of the lines with
//!   idle_ms (rounded to whole seconds) + the file's own age ≥ cold_age_secs are cold. A file that
//!   does not parse is ignored as if missing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::prefetch::{app_cgroups_in, mark_name, uids_in, write_file_in};
use super::server::{workloads, Discovery};
use super::{app_key, disp, h, page_size, read};

pub const AGE_DIR: &str = "/run/ultra-ram-age";
/// Exit code for "this kernel/system cannot do it" — the unit sets RestartPreventExitStatus=3.
const EXIT_UNSUPPORTED: i32 = 3;
/// A fresh lruvec starts with seqs 0..=MIN_NR_GENS+1 (= 3), all created at once without a walk;
/// every later generation boundary comes from aging.
const FIRST_AGED_SEQ: u64 = 4;
const MAGIC: &str = "ultra-ram-age";
const VERSION: &str = "1";
/// LRU_GEN_CORE and LRU_GEN_MM_WALK bits of /sys/kernel/mm/lru_gen/enabled.
const CAP_CORE: u32 = 0x1;
const CAP_MM_WALK: u32 = 0x2;
/// In non-verbose mode, one summary line about every this many seconds.
const SUMMARY_SECS: u64 = 600;

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;

// ───────────────────────────── paths ─────────────────────────────

/// Every filesystem root this module reads or writes, so tests can point it at a temp dir.
pub struct Sys {
    cgroup: PathBuf, // /sys/fs/cgroup
    debug: PathBuf,  // /sys/kernel/debug
    mm: PathBuf,     // /sys/kernel/mm
    proc_: PathBuf,  // /proc
    out: PathBuf,    // AGE_DIR
    /// `--profile server`: age the server workloads (server.rs) instead of the users' apps.
    server: Option<Discovery>,
}

impl Sys {
    pub fn real() -> Sys {
        Sys {
            cgroup: "/sys/fs/cgroup".into(),
            debug: "/sys/kernel/debug".into(),
            mm: "/sys/kernel/mm".into(),
            proc_: "/proc".into(),
            out: AGE_DIR.into(),
            server: None,
        }
    }
    #[cfg(test)]
    fn under(root: &Path) -> Sys {
        Sys {
            cgroup: root.join("sys/fs/cgroup"),
            debug: root.join("sys/kernel/debug"),
            mm: root.join("sys/kernel/mm"),
            proc_: root.join("proc"),
            out: root.join("run/ultra-ram-age"),
            server: None,
        }
    }
    /// The cgroups this helper ages: every user's app cgroups, or the server workloads.
    fn apps(&self) -> Vec<PathBuf> {
        match &self.server {
            Some(d) => workloads(&self.cgroup, d, None),
            None => app_cgroups_in(&self.cgroup),
        }
    }
}

// ───────────────────────────── debugfs lru_gen ─────────────────────────────

/// The debugfs file, injected so tests can fake the kernel.
trait Lru {
    /// Contents of `lru_gen`, or errno.
    fn list(&self) -> Result<String, i32>;
    /// Writes one command to `lru_gen` (one write() call), or errno.
    fn write(&self, cmd: &str) -> Result<(), i32>;
}

struct Real {
    path: PathBuf,
}

fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(EIO)
}

impl Lru for Real {
    fn list(&self) -> Result<String, i32> {
        // Cgroup names are arbitrary bytes; a lossy path only fails to match an app, never panics.
        fs::read(&self.path).map(|b| String::from_utf8_lossy(&b).into_owned()).map_err(|e| errno_of(&e))
    }
    fn write(&self, cmd: &str) -> Result<(), i32> {
        // No O_CREAT/O_TRUNC on a debugfs file: open for writing only.
        let mut f = fs::OpenOptions::new().write(true).open(&self.path).map_err(|e| errno_of(&e))?;
        f.write_all(cmd.as_bytes()).map_err(|e| errno_of(&e))
    }
}

/// Errors after which retrying cannot help: end the service (exit code EXIT_UNSUPPORTED).
fn fatal(e: i32) -> bool {
    matches!(e, EPERM | EACCES | EROFS | ENOENT)
}

fn fatal_msg(path: &Path, e: i32) -> String {
    match e {
        ENOENT => format!(
            "{} yoxdur: debugfs qoşulmayıb (sudo mount -t debugfs none /sys/kernel/debug) və ya kernel CONFIG_LRU_GEN / CONFIG_DEBUG_FS olmadan qurulub",
            path.display()
        ),
        EPERM | EACCES => format!(
            "{}: giriş qadağandır (errno {e}) — root deyil, və ya kernel lockdown rejimi debugfs-ə yazmağı bağlayır",
            path.display()
        ),
        EROFS => format!(
            "{}: yalnız oxunandır — systemd ProtectKernelTunables=yes debugfs-i bağlayır, xidmətdə söndürün",
            path.display()
        ),
        e => format!("{}: gözlənilməz xəta, errno {e}", path.display()),
    }
}

/// One generation of one lruvec, as listed by the kernel. Counts are pages.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Gen {
    seq: u64,
    age_ms: u64,
    anon: u64,
    file: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    nid: u32,
    gens: Vec<Gen>,
}

impl Node {
    fn max_seq(&self) -> Option<u64> {
        self.gens.iter().map(|g| g.seq).max()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Memcg {
    id: u64,
    /// cgroup path relative to the cgroup2 root, starting with `/`.
    path: String,
    nodes: Vec<Node>,
}

/// A page count, optionally followed by the `x` that `lru_gen_full` prints below min_seq.
fn count(tok: &str) -> Option<u64> {
    tok.strip_suffix('x').unwrap_or(tok).parse().ok()
}

/// Parses `lru_gen` (or `lru_gen_full`: its tier and mm-walk lines are skipped). Tolerant: lines
/// it does not understand are ignored, a generation line before any `memcg`/`node` is dropped.
fn parse_listing(text: &str) -> Vec<Memcg> {
    let mut out: Vec<Memcg> = Vec::new();
    // Whether the last `memcg` line parsed: lines after a broken one must not land in the previous memcg.
    let mut cur = false;
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("memcg ") {
            let rest = rest.trim_start();
            let (id, path) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            match id.parse() {
                Ok(id) => {
                    out.push(Memcg { id, path: path.trim().to_string(), nodes: Vec::new() });
                    cur = true;
                }
                Err(_) => cur = false,
            }
            continue;
        }
        if !cur {
            continue;
        }
        let Some(m) = out.last_mut() else { continue };
        if let Some(rest) = l.strip_prefix("node ") {
            if let Ok(nid) = rest.trim().parse() {
                m.nodes.push(Node { nid, gens: Vec::new() });
            }
            continue;
        }
        let t: Vec<&str> = l.split_whitespace().collect();
        // A generation line: seq and age are plain numbers. Tier lines have 7 fields, the
        // mm-walk line has letters glued to its numbers (`0T`), so neither matches.
        if t.len() != 4 {
            continue;
        }
        let (Ok(seq), Ok(age_ms), Some(anon), Some(file)) = (t[0].parse(), t[1].parse(), count(t[2]), count(t[3])) else {
            continue;
        };
        if let Some(n) = m.nodes.last_mut() {
            n.gens.push(Gen { seq, age_ms, anon, file });
        }
    }
    out
}

/// The aging command for one lruvec (see the module docs for `1 1`).
fn age_cmd(id: u64, nid: u32, max_seq: u64) -> String {
    format!("+ {id} {nid} {max_seq} 1 1\n")
}

/// `/sys/kernel/mm/lru_gen/enabled` (`0x0007`) → capability bits. Err: no MGLRU in this kernel.
fn mglru_caps(sys: &Sys) -> Result<u32, String> {
    let p = sys.mm.join("lru_gen/enabled");
    let t = fs::read_to_string(&p).map_err(|_| format!("{} yoxdur: kernel MGLRU-suz qurulub (CONFIG_LRU_GEN)", p.display()))?;
    let t = t.trim();
    u32::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|_| format!("{}: anlaşılmayan dəyər {t:?}", p.display()))
}

// ───────────────────────────── app cgroups → memcgs ─────────────────────────────

/// Every app cgroup (absolute path) with its memcgs from the listing: the cgroup itself and every
/// descendant, since pages are charged to the leaf memcg and `memory.reclaim` on the app covers
/// them all. If a path is listed twice (a dying memcg next to a new one with the same name), the
/// later entry wins: the iterator lists siblings in creation order.
fn map_apps<'a>(root: &Path, apps: &[PathBuf], list: &'a [Memcg]) -> Vec<(PathBuf, Vec<&'a Memcg>)> {
    let mut by_path: BTreeMap<&str, &Memcg> = BTreeMap::new();
    for m in list {
        by_path.insert(m.path.as_str(), m);
    }
    apps.iter()
        .filter_map(|a| {
            let rel = Path::new("/").join(a.strip_prefix(root).ok()?);
            let ms: Vec<&Memcg> = by_path.iter().filter(|(p, _)| Path::new(p).starts_with(&rel)).map(|(_, m)| *m).collect();
            (!ms.is_empty()).then(|| (a.clone(), ms))
        })
        .collect()
}

fn name_of(cg: &Path) -> String {
    disp(&app_key(&cg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
}

// ───────────────────────────── idle map ─────────────────────────────

/// (idle_ms, anon pages) for every generation of one lruvec (definition in the module docs).
fn idle_rows(gens: &[Gen]) -> Vec<(u64, u64)> {
    let Some(max) = gens.iter().map(|g| g.seq).max() else { return Vec::new() };
    gens.iter()
        .map(|g| {
            let b = (g.seq + 1).max(FIRST_AGED_SEQ);
            let idle = if b <= max { gens.iter().find(|x| x.seq == b).map_or(0, |x| x.age_ms) } else { 0 };
            (idle, g.anon)
        })
        .collect()
}

/// Every generation of an app (all its memcgs and nodes) as (idle_ms, anon bytes), oldest first.
fn app_rows(ms: &[&Memcg], page: u64) -> Vec<(u64, u64)> {
    let mut rows: Vec<(u64, u64)> = ms
        .iter()
        .flat_map(|m| m.nodes.iter())
        .flat_map(|n| idle_rows(&n.gens))
        .map(|(idle, pages)| (idle, pages.saturating_mul(page)))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows
}

/// Has a generation idle for `idle_ms` at the time of the file, read `elapsed` seconds later, been
/// idle for `cold_secs`? Compared in whole seconds: generations a fixed number of intervals apart
/// differ from the exact multiple by a few milliseconds of walk time either way.
fn is_cold(idle_ms: u64, elapsed: u64, cold_secs: u64) -> bool {
    (idle_ms + 500) / 1000 + elapsed >= cold_secs
}

fn render(now: u64, interval: u64, rows: &[(u64, u64)]) -> String {
    let mut s = format!("{MAGIC} {VERSION} {now} {interval}\n");
    for (idle, bytes) in rows {
        s.push_str(&format!("{idle} {bytes}\n"));
    }
    s
}

#[derive(Debug, PartialEq)]
struct AgeFile {
    time: u64,
    interval: u64,
    /// (idle_ms, anon bytes)
    rows: Vec<(u64, u64)>,
}

/// Strict parse of an age file: anything unexpected → None (the caller then ignores it).
fn parse_file(text: &str) -> Option<AgeFile> {
    let mut lines = text.lines();
    let hd: Vec<&str> = lines.next()?.split_whitespace().collect();
    if hd.len() != 4 || hd[0] != MAGIC || hd[1] != VERSION {
        return None;
    }
    let (time, interval) = (hd[2].parse().ok()?, hd[3].parse::<u64>().ok()?);
    if interval == 0 {
        return None;
    }
    let mut rows = Vec::new();
    for l in lines {
        if l.trim().is_empty() {
            continue;
        }
        let t: Vec<&str> = l.split_whitespace().collect();
        if t.len() != 2 {
            return None;
        }
        rows.push((t[0].parse().ok()?, t[1].parse().ok()?));
    }
    Some(AgeFile { time, interval, rows })
}

/// The age file of an app cgroup (`<AGE_DIR>/<mark_name(dir)>`), for the daemon.
pub(crate) fn file_for(cg: &Path) -> Option<PathBuf> {
    Some(Path::new(AGE_DIR).join(mark_name(&cg.file_name()?.to_string_lossy())?))
}

/// The daemon's cap: anonymous bytes that have been idle for at least `cold_secs`, from the text
/// of an age file. None when the file is missing, malformed, or not fresh (older than 3 aging
/// intervals, or from the future) — then the daemon behaves as without the helper.
pub(crate) fn cold_bytes(text: &str, now: u64, cold_secs: u64) -> Option<u64> {
    let f = parse_file(text)?;
    let elapsed = now.saturating_sub(f.time);
    if f.time > now.saturating_add(f.interval) || elapsed >= f.interval.saturating_mul(3) {
        return None;
    }
    Some(f.rows.iter().filter(|r| is_cold(r.0, elapsed, cold_secs)).fold(0u64, |s, r| s.saturating_add(r.1)))
}

/// Bytes idle ≥ 2T, idle in [T, 2T), and the rest — the log's one-line map of an app.
fn buckets(rows: &[(u64, u64)], interval: u64) -> (u64, u64, u64) {
    let mut b = (0, 0, 0);
    for &(idle, bytes) in rows {
        if is_cold(idle, 0, 2 * interval) {
            b.0 += bytes;
        } else if is_cold(idle, 0, interval) {
            b.1 += bytes;
        } else {
            b.2 += bytes;
        }
    }
    b
}

// ───────────────────────────── one tick ─────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct Stats {
    /// App cgroups found in the listing.
    apps: usize,
    /// Lruvecs (memcg × node) we sent an aging command for.
    lruvecs: usize,
    aged: usize,
    /// -EEXIST: the kernel's own reclaim was finishing that generation at the same moment.
    concurrent: usize,
    /// -EINVAL answered by re-reading the listing and sending again.
    retried: usize,
    /// The memcg disappeared between listing and aging.
    vanished: usize,
    failed: usize,
    last_err: Option<i32>,
    /// Age files written / skipped because two cgroups share a name.
    files: usize,
    clashes: usize,
}

#[derive(Debug, PartialEq)]
struct AppAge {
    cg: PathBuf,
    lruvecs: usize,
    rows: Vec<(u64, u64)>,
}

/// One aging command; on -EINVAL (stale id or max_seq: raced with another aging, or the memcg was
/// removed or replaced) re-read the listing and try once more. Err(errno): fatal, end the service.
fn age_one(lru: &dyn Lru, m: &Memcg, n: &Node, st: &mut Stats) -> Result<(), i32> {
    let Some(seq) = n.max_seq() else { return Ok(()) };
    st.lruvecs += 1;
    let mut r = lru.write(&age_cmd(m.id, n.nid, seq));
    if r == Err(EINVAL) {
        let fresh = match lru.list() {
            Ok(t) => parse_listing(&t),
            Err(e) if fatal(e) => return Err(e),
            Err(e) => {
                st.failed += 1;
                st.last_err = Some(e);
                return Ok(());
            }
        };
        let again = fresh.iter().rev().find(|x| x.path == m.path).and_then(|x| {
            let s = x.nodes.iter().find(|y| y.nid == n.nid)?.max_seq()?;
            Some((x.id, s))
        });
        let Some((id, s)) = again else {
            st.vanished += 1;
            return Ok(());
        };
        st.retried += 1;
        r = lru.write(&age_cmd(id, n.nid, s));
    }
    match r {
        Ok(()) => st.aged += 1,
        Err(EEXIST) => st.concurrent += 1,
        Err(e) if fatal(e) => return Err(e),
        Err(e) => {
            st.failed += 1;
            st.last_err = Some(e);
        }
    }
    Ok(())
}

/// Writes one age file per app (names shared by two apps get none) and deletes every other file
/// in the directory. Returns the error messages (a failed write only warns).
fn write_files(dir: &Path, ages: &[AppAge], now: u64, interval: u64, st: &mut Stats) -> Vec<String> {
    let mut names: BTreeMap<String, Vec<&AppAge>> = BTreeMap::new();
    for a in ages {
        if let Some(n) = a.cg.file_name().and_then(|n| mark_name(&n.to_string_lossy())) {
            names.entry(n).or_default().push(a);
        }
    }
    let mut errs = Vec::new();
    let mut keep = BTreeSet::new();
    for (name, v) in &names {
        if v.len() > 1 {
            st.clashes += 1;
            continue;
        }
        match write_file_in(dir, name, &render(now, interval, &v[0].rows)) {
            Ok(()) => {
                st.files += 1;
                keep.insert(name.clone());
            }
            Err(e) => errs.push(e),
        }
    }
    // Only when the directory was accepted. Removes the entries themselves (a link, never what it
    // points to), never a temp file in flight.
    if !errs.is_empty() {
        return errs;
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with('.') && !keep.contains(&n) && e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()) {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    errs
}

/// One tick: age every lruvec of every app cgroup, re-read the listing, export the idle map.
/// Err(errno): the listing could not be read, or a fatal error (`fatal`) while aging.
fn tick(sys: &Sys, lru: &dyn Lru, interval: u64, now: impl Fn() -> u64) -> Result<(Stats, Vec<AppAge>, Vec<String>), i32> {
    let mut st = Stats::default();
    let apps = sys.apps();
    let list = parse_listing(&lru.list()?);
    let targets = map_apps(&sys.cgroup, &apps, &list);
    st.apps = targets.len();
    for (_, ms) in &targets {
        for m in ms {
            for n in &m.nodes {
                age_one(lru, m, n, &mut st)?;
            }
        }
    }
    // Ages are read after aging, so the newest generation is the one we just created.
    let list = parse_listing(&lru.list()?);
    let page = page_size();
    let ages: Vec<AppAge> = map_apps(&sys.cgroup, &sys.apps(), &list)
        .into_iter()
        .map(|(cg, ms)| AppAge { lruvecs: ms.iter().map(|m| m.nodes.len()).sum(), rows: app_rows(&ms, page), cg })
        .collect();
    let errs = write_files(&sys.out, &ages, now(), interval, &mut st);
    Ok((st, ages, errs))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn stats_line(st: &Stats, ages: &[AppAge], interval: u64) -> String {
    let (c2, c1, _) = ages.iter().map(|a| buckets(&a.rows, interval)).fold((0, 0, 0), |s, b| (s.0 + b.0, s.1 + b.1, s.2 + b.2));
    let mut s = format!(
        "yaşlandırma: {} tətbiq, {}/{} lruvec qəbul edildi, boş ≥{}s: {}, {}–{}s: {}",
        st.apps,
        st.aged,
        st.lruvecs,
        2 * interval,
        h(c2),
        interval,
        2 * interval,
        h(c1)
    );
    if st.concurrent > 0 {
        s += &format!(", {} paralel (kernel özü yaşlandırırdı)", st.concurrent);
    }
    if st.retried > 0 {
        s += &format!(", {} təkrar", st.retried);
    }
    if st.vanished > 0 {
        s += &format!(", {} yoxa çıxdı", st.vanished);
    }
    if st.failed > 0 {
        s += &format!(", {} xəta (son errno {})", st.failed, st.last_err.unwrap_or(0));
    }
    if st.clashes > 0 {
        s += &format!(", {} ad toqquşması (fayl yazılmadı)", st.clashes);
    }
    s
}

fn service(sys: &Sys, lru: &dyn Lru, o: &Opts) -> i32 {
    println!(
        "ultra-ram age: işə düşdü — hər {} s bütün tətbiq cgroup-ları yaşlandırılır, xəritə: {} (daemon üçün cold_age_secs ≤ {} olmalıdır)",
        o.interval_secs,
        sys.out.display(),
        2 * o.interval_secs
    );
    if let Some(d) = &sys.server {
        println!("server profile: aging the workloads below {} (depth {})", d.roots.join(", "), d.depth);
    }
    let start = Instant::now();
    let period = Duration::from_secs(o.interval_secs);
    let summary_every = (SUMMARY_SECS / o.interval_secs).max(1);
    let mut k: u64 = 0;
    let mut users: Vec<u32> = Vec::new();
    loop {
        let t0 = Instant::now();
        match tick(sys, lru, o.interval_secs, unix_now) {
            Ok((st, ages, errs)) => {
                for e in &errs {
                    println!("! {e}");
                }
                if o.verbose && sys.server.is_none() {
                    let u = uids_in(&sys.cgroup);
                    if u != users {
                        println!("  istifadəçilər: {u:?}");
                        users = u;
                    }
                    for a in &ages {
                        let (c2, c1, c0) = buckets(&a.rows, o.interval_secs);
                        println!(
                            "  {}: {} lruvec, anon {} | boş ≥{}s {}, ≥{}s {}, təzə {}",
                            name_of(&a.cg),
                            a.lruvecs,
                            h(c2 + c1 + c0),
                            2 * o.interval_secs,
                            h(c2),
                            o.interval_secs,
                            h(c1),
                            h(c0)
                        );
                    }
                }
                if o.verbose || k % summary_every == 0 || st.failed > 0 {
                    println!("{} ({} ms)", stats_line(&st, &ages, o.interval_secs), t0.elapsed().as_millis());
                }
            }
            Err(e) if fatal(e) => {
                println!("✗ {}", fatal_msg(&sys.debug.join("lru_gen"), e));
                return EXIT_UNSUPPORTED;
            }
            Err(e) => println!("! {} — növbəti addımda yenidən", fatal_msg(&sys.debug.join("lru_gen"), e)),
        }
        // Fixed rate, so generations are a whole number of intervals apart; after a long stall
        // (suspend) continue from now instead of catching up.
        k += 1;
        let mut next = start + period * k as u32;
        let now = Instant::now();
        if next < now {
            k = (now - start).as_secs() / o.interval_secs + 1;
            next = start + period * k as u32;
        }
        thread::sleep(next - Instant::now().min(next));
    }
}

// ───────────────────────────── status ─────────────────────────────

fn secs(ms: u64) -> String {
    format!("{:.0}s", ms as f64 / 1000.0)
}

fn status(sys: &Sys, lru: &dyn Lru, caps: u32, cold_secs: u64) -> i32 {
    let text = match lru.list() {
        Ok(t) => t,
        Err(e) => {
            println!("✗ {}", fatal_msg(&sys.debug.join("lru_gen"), e));
            return EXIT_UNSUPPORTED;
        }
    };
    let list = parse_listing(&text);
    let targets = map_apps(&sys.cgroup, &sys.apps(), &list);
    println!("═══ MGLRU NƏSİLLƏRİ (soyuq yaddaş xəritəsi) — lru_gen enabled=0x{caps:04x} ═══");
    if caps & CAP_MM_WALK == 0 {
        println!("! Səhifə cədvəli gəzintisi (0x0002) yoxdur: yaşlar boşluq haqqında məlumat vermir.");
    }
    // Is the helper running? The newest age file tells.
    let now = unix_now();
    let newest = fs::read_dir(&sys.out)
        .map(|rd| rd.flatten().filter_map(|e| parse_file(&read(&e.path()))).max_by_key(|f| f.time))
        .ok()
        .flatten();
    match newest {
        Some(f) if now.saturating_sub(f.time) < 3 * f.interval => println!(
            "Köməkçi (age run): işləyir, son yaşlandırma {} s əvvəl, hər {} s",
            now.saturating_sub(f.time),
            f.interval
        ),
        _ => println!("Köməkçi (age run): işləmir — aşağıdakı yaşlar yalnız kernelin öz yaşlandırmasındandır"),
    }
    println!("BOŞ ≥: səhifələrin ən azı nə qədər toxunulmadığı (yalnız yaşlandırma gəzintilərindən sonra məlumdur)\n");
    if targets.is_empty() {
        println!("  Heç bir tətbiq cgroup-u tapılmadı.");
    }
    let page = page_size();
    for (cg, ms) in &targets {
        let rows = app_rows(ms, page);
        let anon: u64 = rows.iter().map(|r| r.1).sum();
        let cold: u64 = rows.iter().filter(|r| is_cold(r.0, 0, cold_secs)).map(|r| r.1).sum();
        println!(
            "  {} [{}]  anon {}, ≥{} s boş: {}",
            name_of(cg),
            cg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            h(anon),
            cold_secs,
            h(cold)
        );
        let many = ms.iter().map(|m| m.nodes.len()).sum::<usize>() > 1;
        for m in ms {
            for n in &m.nodes {
                if many {
                    let rel = Path::new(&m.path).strip_prefix(Path::new("/").join(cg.strip_prefix(&sys.cgroup).unwrap_or(cg)));
                    let sub = rel.ok().map(|r| r.display().to_string()).filter(|s| !s.is_empty()).unwrap_or(".".into());
                    println!("     memcg {} {} node {}", m.id, sub, n.nid);
                }
                println!("     {:>8} {:>9} {:>9} {:>9} {:>9}", "NƏSİL", "YAŞ", "BOŞ ≥", "ANON", "FAYL");
                for (g, (idle, _)) in n.gens.iter().zip(idle_rows(&n.gens)) {
                    println!(
                        "     {:>8} {:>9} {:>9} {:>9} {:>9}",
                        g.seq,
                        secs(g.age_ms),
                        secs(idle),
                        h(g.anon.saturating_mul(page)),
                        h(g.file.saturating_mul(page))
                    );
                }
            }
        }
    }
    0
}

// ───────────────────────────── arguments ─────────────────────────────

#[derive(Debug, PartialEq, Clone)]
struct Opts {
    interval_secs: u64,
    verbose: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { interval_secs: 60, verbose: false }
    }
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Run(Opts),
    Status { cold_secs: u64 },
    Help,
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let run = match args.first().map(|s| s.as_str()) {
        None | Some("help") | Some("--help") | Some("-h") => return Ok(Cmd::Help),
        Some("run") => true,
        Some("status") => false,
        Some(x) => return Err(format!("naməlum əmr: age {x}")),
    };
    fn num<'a>(flag: &str, it: &mut impl Iterator<Item = &'a String>, lo: u64, hi: u64) -> Result<u64, String> {
        let v = it.next().ok_or(format!("{flag} üçün dəyər verilməyib"))?;
        let n: u64 = v.parse().map_err(|_| format!("{flag}: rəqəm gözlənilir, '{v}' verilib"))?;
        if !(lo..=hi).contains(&n) {
            return Err(format!("{flag} {lo}..{hi} aralığında olmalıdır"));
        }
        Ok(n)
    }
    let mut o = Opts::default();
    let mut cold_secs = 120;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--verbose" | "-v" if run => o.verbose = true,
            "--interval-secs" if run => o.interval_secs = num(a, &mut it, 5, 3600)?,
            "--cold-secs" if !run => cold_secs = num(a, &mut it, 1, 86_400)?,
            x => return Err(format!("naməlum parametr: {x}")),
        }
    }
    Ok(if run { Cmd::Run(o) } else { Cmd::Status { cold_secs } })
}

// ───────────────────────────── entry point ─────────────────────────────

fn help() {
    println!(
        "ultra-ram age — [root, istəyə bağlı] tətbiqlərin yaddaşını MGLRU ilə mütəmadi yaşlandırır:
kernel hər tətbiqin səhifə cədvəllərini gəzir, istifadə olunan səhifələri cavan nəslə keçirir,
toxunulmayanlar köhnə nəsildə qalır. Daemon bu xəritə ilə yalnız həqiqətən soyuq yaddaşı köçürür.
(/sys/kernel/debug/lru_gen, MGLRU və debugfs lazımdır)

  run                   Xidmət rejimi: hər intervalda yaşlandır, {AGE_DIR}/ xəritəsini yaz
      --interval-secs T Yaşlandırma intervalı (standart 60). Ən köhnə nəsil ≥ 2T boşdur, ona
                        görə daemonun cold_age_secs ayarı 2T-dən böyük olmamalıdır
      --verbose         Hər addımda hər tətbiqin xəritəsi
  status                Hər tətbiqin nəsilləri: yaş, ən azı nə qədər boş, anon/fayl həcmi
      --cold-secs N     \"Soyuq\" həddi (standart 120)

Jurnal: 'yaşlandırma: …' sətri — neçə lruvec yaşlandırıldı və nə qədər yaddaş ≥2T / T–2T boşdur."
    );
}

fn is_root(sys: &Sys) -> bool {
    read(&sys.proc_.join("self/status"))
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|r| r.split_whitespace().nth(1)?.parse::<u32>().ok())
        == Some(0)
}

/// `ultra-ram age ...`; returns the process exit code.
pub fn run(args: &[String], server: Option<Discovery>) -> i32 {
    let cmd = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ {e}\n");
            help();
            return 2;
        }
    };
    if cmd == Cmd::Help {
        help();
        return 0;
    }
    let sys = Sys { server, ..Sys::real() };
    if !is_root(&sys) {
        println!("✗ Bu əmr root tələb edir (debugfs): sudo ultra-ram age {}", args.join(" "));
        return 1;
    }
    if !sys.cgroup.join("cgroup.controllers").exists() {
        println!("✗ cgroup v2 lazımdır ({} cgroup2 deyil)", sys.cgroup.display());
        return EXIT_UNSUPPORTED;
    }
    let caps = match mglru_caps(&sys) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ {e}");
            return EXIT_UNSUPPORTED;
        }
    };
    if caps & CAP_CORE == 0 {
        println!("✗ MGLRU söndürülüb (enabled=0x{caps:04x}). Yandırmaq: echo y | sudo tee /sys/kernel/mm/lru_gen/enabled");
        return EXIT_UNSUPPORTED;
    }
    let lru = Real { path: sys.debug.join("lru_gen") };
    if let Err(e) = lru.list() {
        println!("✗ {}", fatal_msg(&lru.path, e));
        return EXIT_UNSUPPORTED;
    }
    match cmd {
        Cmd::Run(o) => {
            if caps & CAP_MM_WALK == 0 {
                println!(
                    "✗ lru_gen enabled=0x{caps:04x}: səhifə cədvəli gəzintisi (0x0002) yoxdur — yaşlandırma hansı səhifənin boş olduğunu göstərmir"
                );
                return EXIT_UNSUPPORTED;
            }
            service(&sys, &lru, &o)
        }
        Cmd::Status { cold_secs } => status(&sys, &lru, caps, cold_secs),
        Cmd::Help => 0,
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    const MB: u64 = 1 << 20;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const APP: &str = "/user.slice/user-1000.slice/user@1000.service/app.slice";

    /// `lru_gen` as `lru_gen_seq_show` prints it (v6.18: `memcg %5hu %s`, ` node %5d`,
    /// ` %10lu %10u` + 2 × ` %10lu%c` with a blank mark), two nodes for Firefox.
    fn sample() -> String {
        format!(
            "memcg     1 /
 node     0
         12    9876543       1000      50000
         13    5432100        200       3000
         14      40000         50       1000
         15       1000         10        500
memcg    57 /user.slice
 node     0
          0     100000          0          0
          1     100000          0          0
          2     100000          0          0
          3     100000          0          0
memcg   201 {APP}/app-firefox-1234.scope
 node     0
          5     185000      40000        100
          6     121000       2000        200
          7      61000       3000        300
          8       1200       9000        400
 node     1
          9     183000        100          0
         10     122000        100          0
         11      60000        100          0
memcg   202 {APP}/app-com.google.Chrome@abc.service
 node     0
          2     300000       5000          0
          3     300000        100          0
memcg   203 {APP}/app-com.google.Chrome@abc.service/renderer
 node     0
         20      90000       1000          0
         21          5          1          0
memcg   204 {APP}/app-com.google.Chrome@abcdef.service
 node     0
          4      10000        777          0
"
        )
    }

    #[test]
    fn listing_parser() {
        let l = parse_listing(&sample());
        assert_eq!(l.len(), 6);
        assert_eq!((l[0].id, l[0].path.as_str()), (1, "/"));
        assert_eq!(l[0].nodes[0].gens[0], Gen { seq: 12, age_ms: 9876543, anon: 1000, file: 50000 });
        let ff = &l[2];
        assert_eq!(ff.id, 201);
        assert_eq!(ff.path, format!("{APP}/app-firefox-1234.scope"));
        assert_eq!(ff.nodes.len(), 2);
        assert_eq!(ff.nodes[1].nid, 1);
        assert_eq!(ff.nodes[0].max_seq(), Some(8));
        assert_eq!(ff.nodes[1].max_seq(), Some(11));
        assert_eq!(ff.nodes[1].gens.len(), 3);

        // master prints the id with %llu (no padding); paths may contain spaces.
        let l = parse_listing("memcg 70000 /a b/c d\n node 0\n 4 10 2 3\n");
        assert_eq!((l[0].id, l[0].path.as_str()), (70000, "/a b/c d"));
        assert_eq!(l[0].nodes[0].gens, vec![Gen { seq: 4, age_ms: 10, anon: 2, file: 3 }]);

        // lru_gen_full: `x` marks are accepted, tier and mm-walk lines are skipped.
        let full = "memcg    42 /x
 node     0
          4      30000        100x       200
                     0          0r          0e          0p          1r          2e          3p
                     1          0r          0e          0p          0r          0e          0p
          5       2000          7         8
                     0          9R         10T          0x          0R          0T          0x
                                0T          1Y          2F          3A
";
        let l = parse_listing(full);
        assert_eq!(l[0].nodes[0].gens, vec![
            Gen { seq: 4, age_ms: 30000, anon: 100, file: 200 },
            Gen { seq: 5, age_ms: 2000, anon: 7, file: 8 }
        ]);

        // Garbage: a broken memcg line must not leak its generations into the previous memcg.
        let l = parse_listing("junk\n 1 2 3 4\nmemcg 5 /a\n node 0\n 1 2 3 4\nmemcg zz /b\n node 0\n 9 9 9 9\nnode x\n");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].nodes[0].gens.len(), 1);
        assert!(parse_listing("").is_empty());
    }

    #[test]
    fn command_format() {
        assert_eq!(age_cmd(201, 0, 8), "+ 201 0 8 1 1\n");
        assert_eq!(age_cmd(70000, 3, 123456789), "+ 70000 3 123456789 1 1\n");
    }

    #[test]
    fn idle_definition() {
        let g = |seq, age_ms, anon| Gen { seq, age_ms, anon, file: 0 };
        // Steady state, T = 60 s: oldest idle ≥ age of the next one, youngest 0.
        let r = idle_rows(&[g(5, 185_000, 1), g(6, 121_000, 2), g(7, 61_000, 3), g(8, 1_200, 4)]);
        assert_eq!(r, vec![(121_000, 1), (61_000, 2), (1_200, 3), (0, 4)]);
        // A fresh lruvec (seqs 0..=3, never walked): nothing is known to be idle.
        assert!(idle_rows(&[g(0, 9, 1), g(1, 9, 1), g(2, 9, 1), g(3, 9, 1)]).iter().all(|r| r.0 == 0));
        // After the first aging (seqs 1..=4): only gens below the walk boundary 4 are idle, by its age.
        let r = idle_rows(&[g(1, 50_000, 1), g(2, 50_000, 1), g(3, 50_000, 1), g(4, 20_000, 1)]);
        assert_eq!(r.iter().map(|x| x.0).collect::<Vec<_>>(), vec![20_000, 20_000, 20_000, 0]);
        assert!(idle_rows(&[]).is_empty());
    }

    #[test]
    fn path_to_memcg_mapping() {
        let root = PathBuf::from("/sys/fs/cgroup");
        let app = |d: &str| root.join(format!("{}/{d}", &APP[1..]));
        let apps = vec![
            app("app-com.google.Chrome@abc.service"),
            app("app-firefox-1234.scope"),
            app("app-gone-9.scope"),
        ];
        let l = parse_listing(&sample());
        let m = map_apps(&root, &apps, &l);
        assert_eq!(m.len(), 2, "an app without a memcg is left out");
        assert_eq!(m[0].0, apps[0]);
        // The app and its child, not the sibling whose name only shares a prefix.
        assert_eq!(m[0].1.iter().map(|x| x.id).collect::<Vec<_>>(), vec![202, 203]);
        assert_eq!(m[1].1.iter().map(|x| x.id).collect::<Vec<_>>(), vec![201]);

        // Same path twice (dying + new memcg): the later one wins.
        let dup = format!("memcg 7 {APP}/app-x.scope\n node 0\n 4 1 1 1\nmemcg 9 {APP}/app-x.scope\n node 0\n 4 1 1 1\n");
        let l = parse_listing(&dup);
        let m = map_apps(&root, &[app("app-x.scope")], &l);
        assert_eq!(m[0].1.iter().map(|x| x.id).collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn cap_computation() {
        let now = 1_700_000_000;
        let file = render(now, 60, &[(121_000, 300 * MB), (119_600, 7 * MB), (61_000, 50 * MB), (0, 900 * MB)]);
        // 119.6 s rounds to 120 s: walk-time jitter must not flip the oldest generations.
        assert_eq!(cold_bytes(&file, now, 120), Some(307 * MB));
        assert_eq!(cold_bytes(&file, now, 30), Some(357 * MB));
        assert_eq!(cold_bytes(&file, now, 3600), Some(0));
        // Read 59 s later: everything has been idle 59 s longer (accesses since then are caught
        // by reclaim's reference check).
        assert_eq!(cold_bytes(&file, now + 59, 120), Some(357 * MB));
        // Stale (≥ 3 intervals old) or from the future → ignored.
        assert!(cold_bytes(&file, now + 179, 120).is_some());
        assert_eq!(cold_bytes(&file, now + 180, 120), None);
        assert_eq!(cold_bytes(&file, now - 61, 120), None);
        assert_eq!(cold_bytes(&file, now - 60, 120), Some(307 * MB));
        // Tolerant parsing: anything odd means "no file".
        assert_eq!(cold_bytes("", now, 120), None);
        assert_eq!(cold_bytes("ultra-ram-age 1 x 60\n", now, 120), None);
        assert_eq!(cold_bytes(&format!("ultra-ram-age 2 {now} 60\n"), now, 120), None);
        assert_eq!(cold_bytes(&format!("ultra-ram-age 1 {now} 0\n"), now, 120), None);
        assert_eq!(cold_bytes(&format!("ultra-ram-age 1 {now} 60\n1 2 3\n"), now, 120), None);
        assert_eq!(cold_bytes(&format!("ultra-ram-age 1 {now} 60\n-1 2\n"), now, 120), None);
        assert_eq!(cold_bytes(&format!("ultra-ram-age 1 {now} 60\n"), now, 120), Some(0));
        assert_eq!(cold_bytes(&format!("ultra-ram-age 1 {now} 60\n200000 {}\n200000 5\n", u64::MAX), now, 120), Some(u64::MAX));
        assert_eq!(file_for(Path::new("/x/app-a b.scope")), Some(PathBuf::from("/run/ultra-ram-age/app-a_b.scope")));
        assert_eq!(file_for(Path::new("/")), None);
    }

    #[test]
    fn buckets_split() {
        let rows = [(121_000, 1), (119_600, 2), (61_000, 4), (59_000, 8), (0, 16)];
        assert_eq!(buckets(&rows, 60), (3, 4, 24));
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(&s(&[])), Ok(Cmd::Help));
        assert_eq!(parse_args(&s(&["help"])), Ok(Cmd::Help));
        assert_eq!(parse_args(&s(&["run"])), Ok(Cmd::Run(Opts::default())));
        assert_eq!(Opts::default().interval_secs, 60);
        assert_eq!(
            parse_args(&s(&["run", "--interval-secs", "20", "--verbose"])),
            Ok(Cmd::Run(Opts { interval_secs: 20, verbose: true }))
        );
        assert_eq!(parse_args(&s(&["run", "-v"])), Ok(Cmd::Run(Opts { interval_secs: 60, verbose: true })));
        assert!(parse_args(&s(&["run", "--interval-secs", "4"])).is_err());
        assert!(parse_args(&s(&["run", "--interval-secs", "3601"])).is_err());
        assert!(parse_args(&s(&["run", "--interval-secs"])).is_err());
        assert!(parse_args(&s(&["run", "--interval-secs", "x"])).is_err());
        assert!(parse_args(&s(&["run", "--cold-secs", "10"])).is_err());
        assert!(parse_args(&s(&["run", "firefox"])).is_err());
        assert_eq!(parse_args(&s(&["status"])), Ok(Cmd::Status { cold_secs: 120 }));
        assert_eq!(parse_args(&s(&["status", "--cold-secs", "300"])), Ok(Cmd::Status { cold_secs: 300 }));
        assert!(parse_args(&s(&["status", "--verbose"])).is_err());
        assert!(parse_args(&s(&["status", "--cold-secs", "0"])).is_err());
        assert!(parse_args(&s(&["bogus"])).is_err());
    }

    // ── fake kernel + filesystem ──

    /// A tiny stateful lru_gen: aging appends a generation (merging the oldest beyond 4), errors
    /// can be scripted per memcg id, memcgs can be replaced or removed between calls.
    struct FakeLru {
        memcgs: RefCell<Vec<Memcg>>,
        errs: RefCell<HashMap<u64, Vec<i32>>>,
        log: RefCell<Vec<String>>,
        /// Applied after the first listing: lets a test race the kernel against us.
        after_first_list: RefCell<Option<Box<dyn FnOnce(&mut Vec<Memcg>)>>>,
    }

    impl FakeLru {
        fn new(memcgs: Vec<Memcg>) -> FakeLru {
            FakeLru {
                memcgs: RefCell::new(memcgs),
                errs: RefCell::new(HashMap::new()),
                log: RefCell::new(Vec::new()),
                after_first_list: RefCell::new(None),
            }
        }
    }

    fn render_listing(ms: &[Memcg]) -> String {
        let mut s = String::new();
        for m in ms {
            s += &format!("memcg {:5} {}\n", m.id, m.path);
            for n in &m.nodes {
                s += &format!(" node {:5}\n", n.nid);
                for g in &n.gens {
                    s += &format!(" {:10} {:10} {:10}  {:10} \n", g.seq, g.age_ms, g.anon, g.file);
                }
            }
        }
        s
    }

    impl Lru for FakeLru {
        fn list(&self) -> Result<String, i32> {
            let out = render_listing(&self.memcgs.borrow());
            if let Some(f) = self.after_first_list.borrow_mut().take() {
                f(&mut self.memcgs.borrow_mut());
            }
            Ok(out)
        }
        fn write(&self, cmd: &str) -> Result<(), i32> {
            self.log.borrow_mut().push(cmd.to_string());
            let t: Vec<u64> = cmd.trim().trim_start_matches("+ ").split(' ').map(|x| x.parse().unwrap()).collect();
            let (id, nid, seq) = (t[0], t[1] as u32, t[2]);
            if let Some(e) = self.errs.borrow_mut().get_mut(&id).and_then(|v| v.pop()) {
                return Err(e);
            }
            let mut ms = self.memcgs.borrow_mut();
            let m = ms.iter_mut().find(|m| m.id == id).ok_or(EINVAL)?;
            let n = m.nodes.iter_mut().find(|n| n.nid == nid).ok_or(EINVAL)?;
            let max = n.max_seq().ok_or(EINVAL)?;
            if seq != max {
                return Err(EINVAL);
            }
            for g in n.gens.iter_mut() {
                g.age_ms += 60_000;
            }
            n.gens.push(Gen { seq: max + 1, age_ms: 0, anon: 0, file: 0 });
            if n.gens.len() > 4 {
                let old = n.gens.remove(0);
                n.gens[0].anon += old.anon;
                n.gens[0].file += old.file;
            }
            Ok(())
        }
    }

    struct Fake {
        root: PathBuf,
        sys: Sys,
    }

    impl Fake {
        fn new(name: &str) -> Fake {
            let root = std::env::temp_dir().join(format!("ultra-ram-age-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("sys/fs/cgroup/user.slice")).unwrap();
            fs::create_dir_all(root.join("run")).unwrap();
            Fake { sys: Sys::under(&root), root }
        }
        fn app(&self, uid: u32, dir: &str) -> PathBuf {
            let p = self.sys.cgroup.join(format!("user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{dir}"));
            fs::create_dir_all(&p).unwrap();
            p
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn memcg(id: u64, path: String, nids: &[u32], first_seq: u64) -> Memcg {
        let gens = (0..4).map(|i| Gen { seq: first_seq + i, age_ms: (3 - i) * 60_000 + 500, anon: 1000 * (4 - i), file: 0 }).collect::<Vec<_>>();
        Memcg { id, path, nodes: nids.iter().map(|&nid| Node { nid, gens: gens.clone() }).collect() }
    }

    #[test]
    fn tick_end_to_end() {
        let f = Fake::new("tick");
        let ff = f.app(1000, "app-firefox-1234.scope");
        let tg = f.app(1000, "app-telegram-5.scope");
        let gone = f.app(1000, "app-gone-7.scope");
        let replaced = f.app(1000, "app-replaced-8.scope");
        // The same instance name for two users: no file for either.
        let a1 = f.app(1000, "app-kalendar@autostart.service");
        let a2 = f.app(1001, "app-kalendar@autostart.service");
        let rel = |p: &Path| format!("/{}", p.strip_prefix(&f.sys.cgroup).unwrap().display());
        let lru = FakeLru::new(vec![
            memcg(1, "/".into(), &[0], 100),
            memcg(10, rel(&ff), &[0, 1], 10),
            memcg(11, format!("{}/child", rel(&ff)), &[0], 4),
            memcg(20, rel(&tg), &[0], 50),
            memcg(30, rel(&gone), &[0], 5),
            memcg(40, rel(&replaced), &[0], 5),
            memcg(50, rel(&a1), &[0], 5),
            memcg(51, rel(&a2), &[0], 5),
        ]);
        // Telegram: the kernel's own reclaim is finishing that generation (-EEXIST).
        lru.errs.borrow_mut().insert(20, vec![EEXIST]);
        // Right after our first listing: `gone` is removed, `replaced` is recreated with a new id.
        let (rg, rr) = (rel(&gone), rel(&replaced));
        *lru.after_first_list.borrow_mut() = Some(Box::new(move |ms: &mut Vec<Memcg>| {
            ms.retain(|m| m.path != rg);
            let m = ms.iter_mut().find(|m| m.path == rr).unwrap();
            m.id = 41;
            m.nodes[0].gens.iter_mut().for_each(|g| g.seq += 100);
        }));
        // Leftovers from earlier ticks: a vanished app's file goes, a temp file in flight stays.
        fs::create_dir_all(&f.sys.out).unwrap();
        fs::set_permissions(&f.sys.out, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(f.sys.out.join("app-old-1.scope"), "x").unwrap();
        fs::write(f.sys.out.join(".app-x.tmp"), "x").unwrap();

        let (st, ages, errs) = tick(&f.sys, &lru, 60, || 1_700_000_000).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(st.apps, 6, "the root memcg is not an app");
        // firefox 2 nodes + child, telegram, gone, replaced, 2 × kalendar.
        assert_eq!(st.lruvecs, 8);
        assert_eq!(st.concurrent, 1);
        assert_eq!(st.vanished, 1);
        assert_eq!(st.retried, 1);
        assert_eq!(st.aged, 6);
        assert_eq!((st.failed, st.files, st.clashes), (0, 3, 1));
        let log = lru.log.borrow();
        assert!(log.contains(&"+ 10 0 13 1 1\n".to_string()) && log.contains(&"+ 10 1 13 1 1\n".to_string()));
        assert!(log.contains(&"+ 11 0 7 1 1\n".to_string()));
        assert!(log.contains(&"+ 41 0 108 1 1\n".to_string()), "retried with the new id and max_seq: {log:?}");
        assert!(!log.iter().any(|c| c.starts_with("+ 1 ")), "root memcg never aged");

        // Firefox's file: its own 2 nodes + the child, 4 generations each, oldest first.
        let txt = fs::read_to_string(f.sys.out.join("app-firefox-1234.scope")).unwrap();
        let af = parse_file(&txt).unwrap();
        assert_eq!((af.time, af.interval), (1_700_000_000, 60));
        assert_eq!(af.rows.len(), 12);
        assert!(af.rows.windows(2).all(|w| w[0].0 >= w[1].0), "oldest first");
        // After aging: seqs 11..=14 with ages 180.5, 120.5, 60.5, 0 s → idle 120.5, 60.5, 0, 0.
        let page = page_size();
        assert_eq!(af.rows[0], (120_500, (4000 + 3000) * page), "oldest gen absorbed the merged one");
        assert_eq!(cold_bytes(&txt, 1_700_000_000, 120), Some(3 * 7000 * page));
        assert_eq!(fs::metadata(f.sys.out.join("app-firefox-1234.scope")).unwrap().permissions().mode() & 0o777, 0o644);
        assert!(f.sys.out.join("app-telegram-5.scope").exists());
        assert!(f.sys.out.join("app-replaced-8.scope").exists());
        assert!(!f.sys.out.join("app-gone-7.scope").exists());
        assert!(!f.sys.out.join("app-kalendar@autostart.service").exists());
        assert!(!f.sys.out.join("app-old-1.scope").exists(), "stale file pruned");
        assert!(f.sys.out.join(".app-x.tmp").exists());
        let fa = ages.iter().find(|a| a.cg == ff).unwrap();
        assert_eq!(fa.lruvecs, 3);

        // Fatal errors end the service; ordinary ones are counted.
        let lru = FakeLru::new(vec![memcg(10, rel(&ff), &[0], 10)]);
        lru.errs.borrow_mut().insert(10, vec![EPERM]);
        assert_eq!(tick(&f.sys, &lru, 60, || 0).err(), Some(EPERM));
        let lru = FakeLru::new(vec![memcg(10, rel(&ff), &[0], 10)]);
        lru.errs.borrow_mut().insert(10, vec![12]);
        let (st, _, _) = tick(&f.sys, &lru, 60, || 0).unwrap();
        assert_eq!((st.failed, st.last_err), (1, Some(12)));
        // EINVAL twice: counted as a failure after the one retry.
        let lru = FakeLru::new(vec![memcg(10, rel(&ff), &[0], 10)]);
        lru.errs.borrow_mut().insert(10, vec![EINVAL, EINVAL]);
        let (st, _, _) = tick(&f.sys, &lru, 60, || 0).unwrap();
        assert_eq!((st.retried, st.failed, st.last_err), (1, 1, Some(EINVAL)));
    }

    #[test]
    fn server_scope() {
        let f = Fake::new("server");
        let svc = f.sys.cgroup.join("system.slice/web-1.service");
        let pod = f.sys.cgroup.join("kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod0a1b2c3d_4e5f.slice");
        fs::create_dir_all(pod.join("cri-containerd-abc.scope")).unwrap();
        fs::create_dir_all(&svc).unwrap();
        let app = f.app(1000, "app-firefox-1.scope");
        let rel = |p: &Path| format!("/{}", p.strip_prefix(&f.sys.cgroup).unwrap().display());
        let memcgs = || {
            vec![
                memcg(10, rel(&svc), &[0], 10),
                memcg(11, rel(&app), &[0], 10),
                memcg(12, rel(&pod), &[0], 10),
                memcg(13, format!("{}/cri-containerd-abc.scope", rel(&pod)), &[0], 10),
            ]
        };
        // --profile server: the workloads (the pod with its container), never the user's app.
        let sys = Sys { server: Some(Discovery::default()), ..Sys::under(&f.root) };
        let (st, ages, errs) = tick(&sys, &FakeLru::new(memcgs()), 60, || 1_700_000_000).unwrap();
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(st.apps, 2);
        assert_eq!(ages.iter().map(|a| a.cg.clone()).collect::<Vec<_>>(), vec![pod.clone(), svc.clone()]);
        assert_eq!(ages[0].lruvecs, 2, "the pod's container memcg is aged with it");
        assert!(sys.out.join("web-1.service").exists());
        assert!(!sys.out.join("app-firefox-1.scope").exists());
        // Without the flag: exactly the desktop behaviour.
        let (st, ages, _) = tick(&f.sys, &FakeLru::new(memcgs()), 60, || 1_700_000_000).unwrap();
        assert_eq!(st.apps, 1);
        assert_eq!(ages[0].cg, app);
    }

    #[test]
    fn files_round_trip_and_safety() {
        let f = Fake::new("files");
        let rows = vec![(121_000, 5 * MB), (61_000, MB), (0, 2 * MB)];
        let ages = vec![AppAge { cg: PathBuf::from("/cg/app-a.scope"), lruvecs: 1, rows: rows.clone() }];
        let mut st = Stats::default();
        assert!(write_files(&f.sys.out, &ages, 1000, 60, &mut st).is_empty());
        let txt = fs::read_to_string(f.sys.out.join("app-a.scope")).unwrap();
        assert_eq!(txt, "ultra-ram-age 1 1000 60\n121000 5242880\n61000 1048576\n0 2097152\n");
        assert_eq!(parse_file(&txt), Some(AgeFile { time: 1000, interval: 60, rows }));
        assert_eq!(fs::metadata(&f.sys.out).unwrap().permissions().mode() & 0o777, 0o755);

        // A symlink planted at the target is replaced, never written through.
        let victim = f.root.join("victim");
        fs::write(&victim, "keep").unwrap();
        fs::remove_file(f.sys.out.join("app-a.scope")).unwrap();
        std::os::unix::fs::symlink(&victim, f.sys.out.join("app-a.scope")).unwrap();
        assert!(write_files(&f.sys.out, &ages, 1001, 60, &mut st).is_empty());
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!fs::symlink_metadata(f.sys.out.join("app-a.scope")).unwrap().file_type().is_symlink());

        // A world-writable output directory is refused, and nothing in it is pruned.
        fs::set_permissions(&f.sys.out, fs::Permissions::from_mode(0o777)).unwrap();
        fs::write(f.sys.out.join("app-other.scope"), "x").unwrap();
        let none: Vec<AppAge> = vec![AppAge { cg: PathBuf::from("/cg/app-b.scope"), lruvecs: 1, rows: vec![] }];
        assert_eq!(write_files(&f.sys.out, &none, 1002, 60, &mut st).len(), 1);
        assert!(f.sys.out.join("app-other.scope").exists());
    }

    #[test]
    fn caps_and_root() {
        let f = Fake::new("caps");
        assert!(mglru_caps(&f.sys).is_err());
        fs::create_dir_all(f.sys.mm.join("lru_gen")).unwrap();
        fs::write(f.sys.mm.join("lru_gen/enabled"), "0x0007\n").unwrap();
        assert_eq!(mglru_caps(&f.sys), Ok(7));
        fs::write(f.sys.mm.join("lru_gen/enabled"), "0x0005\n").unwrap();
        assert_eq!(mglru_caps(&f.sys).map(|c| c & CAP_MM_WALK), Ok(0));
        fs::write(f.sys.mm.join("lru_gen/enabled"), "y\n").unwrap();
        assert!(mglru_caps(&f.sys).is_err());
        assert!(!is_root(&f.sys));
        fs::create_dir_all(f.sys.proc_.join("self")).unwrap();
        fs::write(f.sys.proc_.join("self/status"), "Name: x\nUid:\t0\t0\t0\t0\n").unwrap();
        assert!(is_root(&f.sys));
        assert!(fatal(ENOENT) && fatal(EACCES) && fatal(EROFS) && !fatal(EINVAL) && !fatal(EEXIST));
    }
}
