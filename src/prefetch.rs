//! Optional "prefetch" helper: swap an app's offloaded memory back in *ahead* of it.
//!
//! Problem: when the user switches back to an app whose cold memory sits in zram, the app
//! faults its pages back one by one on its own thread (zram is synchronous and bypasses swap
//! readahead), so a gigabyte of offloaded memory comes back over many seconds. This helper
//! notices that a cgroup has started swapping in and asks the kernel to bring the rest of its
//! swapped-out anonymous memory back in parallel, from several worker threads.
//!
//! Like the zram tier it needs root and lives in its own subcommands (`ultra-ram prefetch ...`);
//! the reclaim daemon itself stays a rootless user service.
//!
//! Kernel interface (checked against mm/madvise.c, mm/memory.c, mm/swap_state.c, mm/page_io.c
//! and Documentation/admin-guide/cgroup-v2.rst):
//! * `process_madvise(pidfd, iov, n, MADV_WILLNEED, 0)` is accepted for another process since
//!   Linux 5.15 (`process_madvise_behavior_valid`, later `process_madvise_remote_valid`);
//!   5.10–5.14 return -EINVAL, older kernels -ENOSYS. The caller needs CAP_SYS_NICE and
//!   PTRACE_MODE_READ over the target (root has both). At most UIO_MAXIOV (1024) iovecs.
//! * For anonymous memory `madvise_willneed` walks the page tables (`swapin_walk_pmd_entry`) and
//!   calls `read_swap_cache_async` for every swap PTE. zram reads synchronously in the caller's
//!   context, so the decompression runs on *our* worker thread — hence several workers.
//!   The pages land in the swap cache (charged to the app's cgroup, not to us) but are not mapped:
//!   the app's next access is a cheap minor fault instead of a decompression.
//! * Accounting consequences: `workingset_refault_anon` grows for our own swap-ins too, but
//!   `pgmajfault` is only counted on the app's fault path when the page was not in the swap
//!   cache. `memory.swap.current` does not drop until the app touches a page (swap cache is
//!   "accounted against both memory and swap usage"), so a wake is measured as the drop of
//!   `memory.swap.current` plus the rise of `swapcached` in memory.stat.
//! * `vector_madvise` holds the target's mmap lock (read) across all iovecs of one call, so one
//!   call never covers more than ~CHUNK of swapped memory.
//!
//! Marker files for the reclaim daemon (`MARK_DIR`, written after every wake that ran):
//! our WILLNEED swap-ins raise the woken cgroup's `workingset_refault_anon` just like the app's
//! own refaults, which the daemon would otherwise read as wasted reclaim. Format, for whoever
//! adds the daemon side (the daemon does not read these yet):
//! * path: `/run/ultra-ram-prefetch/<name>`, `<name>` = the woken cgroup's directory name (e.g.
//!   `app-firefox-1234.scope`, the last path component only, so the same for every user) with
//!   every character outside `[A-Za-z0-9._@-]` and a leading `.` replaced by `_`; names longer
//!   than 200 bytes get no marker. Directory 0755 and files 0644, owned by root; a file is
//!   replaced atomically (rename) on each wake; markers older than MARK_TTL are deleted.
//! * content: one line `<unix_seconds> <workingset_refault_anon>\n` — wall-clock time the wake
//!   finished and the cgroup's memory.stat `workingset_refault_anon` right after it.
//! * suggested use: refault growth between the daemon's previous sample and the marker value,
//!   when the timestamp is newer than that sample, is prefetch, not the app; only growth beyond
//!   the marker value counts against reclaim.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::server::{workloads, Discovery};
use super::{app_key, disp, kv, page_size, pids_in, psi, read, read_u64};

const MB: u64 = 1 << 20;
/// Swapped memory per work item (one process_madvise call, one mmap-lock hold).
const CHUNK: u64 = 4 * MB;
/// UIO_MAXIOV: process_madvise rejects larger vectors with -EINVAL.
const IOV_MAX: usize = 1024;
/// Kept free on top of the MemAvailable floor when sizing a wake.
const MARGIN: u64 = 128 * MB;
/// After a skipped wake (low RAM, PSI, nothing found) look again this soon.
const SKIP_RETRY_MS: u64 = 5000;
/// How often the valves (MemAvailable, PSI) are re-checked while workers run.
const VALVE_POLL: Duration = Duration::from_millis(100);
const MAX_THREADS: u64 = 64;
/// Exit code for "this kernel/arch cannot do it" — the unit sets RestartPreventExitStatus=3.
const EXIT_UNSUPPORTED: i32 = 3;
const MARK_DIR: &str = "/run/ultra-ram-prefetch";
/// Marker files not refreshed for this long belong to long-gone cgroups.
const MARK_TTL: Duration = Duration::from_secs(86_400);

const EPERM: i32 = 1;
const ESRCH: i32 = 3;
const EBADF: i32 = 9;
const EACCES: i32 = 13;
const EINVAL: i32 = 22;
const ENOSYS: i32 = 38;

const MADV_WILLNEED: i64 = 3;
/// (pidfd_open, process_madvise); the generic syscall table numbers, shared by x86_64 and arm64.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const NR: Option<(i64, i64)> = Some((434, 440));
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
const NR: Option<(i64, i64)> = None;

extern "C" {
    /// glibc/musl `syscall(2)`: returns -1 and sets errno on failure. std already links libc.
    fn syscall(num: i64, ...) -> i64;
}

// ───────────────────────────── paths ─────────────────────────────

/// Every filesystem root this module reads, so tests can point it at a temp dir.
pub struct Sys {
    cgroup: PathBuf, // /sys/fs/cgroup
    proc_: PathBuf,  // /proc
    marks: PathBuf,  // MARK_DIR
    /// `--profile server`: watch the server workloads (server.rs) instead of the users' apps.
    server: Option<Discovery>,
}

impl Sys {
    pub fn real() -> Sys {
        Sys { cgroup: "/sys/fs/cgroup".into(), proc_: "/proc".into(), marks: MARK_DIR.into(), server: None }
    }
    #[cfg(test)]
    fn under(root: &Path) -> Sys {
        Sys {
            cgroup: root.join("sys/fs/cgroup"),
            proc_: root.join("proc"),
            marks: root.join("run/ultra-ram-prefetch"),
            server: None,
        }
    }
}

// ───────────────────────────── syscalls ─────────────────────────────

/// `struct iovec`: two pointer-sized words on every 64-bit Linux ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct IoVec {
    base: usize,
    len: usize,
}

/// The three kernel calls a wake makes. Injected so tests can fake them.
trait Kernel: Sync {
    fn pidfd_open(&self, pid: u32) -> Result<i32, i32>;
    fn close(&self, fd: i32);
    /// process_madvise(fd, iov, MADV_WILLNEED): bytes advised or errno.
    fn willneed(&self, fd: i32, iov: &[IoVec]) -> Result<usize, i32>;
    /// The startup probe proved that this kernel accepts remote MADV_WILLNEED.
    fn verified(&self) -> bool;
}

struct Real {
    verified: bool,
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

impl Kernel for Real {
    fn pidfd_open(&self, pid: u32) -> Result<i32, i32> {
        let Some((nr, _)) = NR else { return Err(ENOSYS) };
        // SAFETY: pidfd_open(pid_t, unsigned int) takes no pointers.
        let r = unsafe { syscall(nr, pid as i64, 0i64) };
        if r < 0 {
            Err(errno())
        } else {
            Ok(r as i32)
        }
    }
    fn close(&self, fd: i32) {
        // SAFETY: fd came from pidfd_open above and is closed exactly once (FdSet::drop).
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    fn willneed(&self, fd: i32, iov: &[IoVec]) -> Result<usize, i32> {
        let Some((_, nr)) = NR else { return Err(ENOSYS) };
        // SAFETY: iov points to iov.len() valid iovec structs for the duration of the call; the
        // kernel only reads them (the addresses inside belong to the target process).
        let r = unsafe { syscall(nr, fd as i64, iov.as_ptr() as i64, iov.len() as i64, MADV_WILLNEED, 0i64) };
        if r < 0 {
            Err(errno())
        } else {
            Ok(r as usize)
        }
    }
    fn verified(&self) -> bool {
        self.verified
    }
}

/// What a process_madvise errno means for the wake.
#[derive(Debug, PartialEq, Clone, Copy)]
enum ErrClass {
    /// Leave this process alone for the rest of the wake.
    SkipProc,
    /// End the service: retrying cannot help (exit code EXIT_UNSUPPORTED).
    Fatal,
    /// Only this range failed (e.g. ENOMEM: unmapped meanwhile); go on with the next.
    SkipRange,
}

fn classify(e: i32, verified: bool) -> ErrClass {
    match e {
        // Exited, or (EACCES) this one process denies ptrace-read access.
        ESRCH | EACCES => ErrClass::SkipProc,
        EPERM | ENOSYS => ErrClass::Fatal,
        // Page-aligned anonymous ranges never give EINVAL, so before the probe confirmed support
        // it means the kernel refuses remote WILLNEED (5.10–5.14). Once the probe succeeded, it
        // (or EBADF from its pidfd) concerns this one process only: never end the service for it.
        EINVAL | EBADF if verified => ErrClass::SkipProc,
        EINVAL | EBADF => ErrClass::Fatal,
        _ => ErrClass::SkipRange,
    }
}

fn fatal_msg(e: i32) -> String {
    match e {
        ENOSYS => "kernel pidfd_open/process_madvise dəstəkləmir (Linux ≥ 5.15 lazımdır)".into(),
        EINVAL => "kernel başqa prosesə process_madvise(MADV_WILLNEED) qəbul etmir (Linux ≥ 5.15 lazımdır)".into(),
        EPERM => "icazə yoxdur: process_madvise CAP_SYS_NICE tələb edir (root kimi işə salın)".into(),
        e => format!("gözlənilməz xəta: process_madvise errno {e}"),
    }
}

/// One harmless zero-length WILLNEED on PID 1: tells "kernel too old" apart from runtime errors
/// before the service starts. Our own mm would skip the remote-behaviour check, PID 1 does not.
/// Ok(Some(warning)): PID 1 refused the ptrace-read check (EACCES — a sandbox or an LSM), so
/// support could not be confirmed; the first real call will tell (EINVAL/ENOSYS end the service).
fn probe(k: &dyn Kernel) -> Result<Option<String>, String> {
    let fd = k.pidfd_open(1).map_err(|e| match e {
        ENOSYS => "kernel pidfd_open dəstəkləmir (Linux ≥ 5.15 lazımdır)".to_string(),
        e => format!("pidfd_open(1) alınmadı: errno {e}"),
    })?;
    let r = k.willneed(fd, &[IoVec { base: 0, len: 0 }]);
    k.close(fd);
    match r {
        Ok(_) => Ok(None),
        Err(EACCES) => Ok(Some("PID 1 üzərində yoxlama mümkün olmadı (EACCES) — kernel dəstəyi ilk oyandırmada yoxlanacaq".into())),
        Err(e) => Err(fatal_msg(e)),
    }
}

/// pidfds of one wake, closed when dropped. `None`: the process was already gone.
struct FdSet<'a> {
    k: &'a dyn Kernel,
    fds: Vec<Option<i32>>,
}

impl<'a> FdSet<'a> {
    fn open(k: &'a dyn Kernel, pids: &[u32]) -> Result<FdSet<'a>, String> {
        let mut s = FdSet { k, fds: Vec::with_capacity(pids.len()) };
        for &pid in pids {
            match k.pidfd_open(pid) {
                Ok(fd) => s.fds.push(Some(fd)),
                Err(ENOSYS) => return Err(fatal_msg(ENOSYS)),
                // ESRCH (exited), EINVAL (not a thread-group leader), EMFILE ...: skip this one.
                Err(_) => s.fds.push(None),
            }
        }
        Ok(s)
    }
    /// Closes and forgets the pidfd at `i` (the process will be skipped).
    fn drop_at(&mut self, i: usize) {
        if let Some(fd) = self.fds.get_mut(i).and_then(Option::take) {
            self.k.close(fd);
        }
    }
}

impl Drop for FdSet<'_> {
    fn drop(&mut self) {
        for fd in self.fds.iter().flatten() {
            self.k.close(*fd);
        }
    }
}

// ───────────────────────────── /proc/PID/smaps ─────────────────────────────

/// A private anonymous mapping with some of it in swap.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    start: u64,
    end: u64,
    swap: u64,
}

/// VmFlags that rule a mapping out: shared, hugetlb, I/O, PFN maps, "do not dump" (driver or
/// app-private special memory).
const SKIP_FLAGS: [&str; 5] = ["sh", "ht", "io", "pf", "dd"];

fn is_anon_name(name: &str) -> bool {
    name.is_empty() || name == "[heap]" || name == "[stack]" || name.starts_with("[stack:") || name.starts_with("[anon:")
}

/// `start-end perms offset dev inode [name]` → (region, eligible so far).
fn parse_header(line: &str) -> Option<(Region, bool)> {
    let mut it = line.split_whitespace();
    let (a, b) = it.next()?.split_once('-')?;
    let start = u64::from_str_radix(a, 16).ok()?;
    let end = u64::from_str_radix(b, 16).ok()?;
    let perms = it.next()?;
    let _offset = it.next()?;
    let _dev = it.next()?;
    let inode = it.next()?;
    let name = it.collect::<Vec<_>>().join(" ");
    // Private (copy-on-write) and not backed by a file: on kernels before 6.x WILLNEED on a file
    // mapping would start file readahead instead of a swap-in.
    let ok = perms.as_bytes().get(3) == Some(&b'p') && inode == "0" && is_anon_name(&name);
    Some((Region { start, end, swap: 0 }, ok))
}

/// Private anonymous regions (`[heap]`, `[stack]`, `[anon:*]`, unnamed) with `Swap:` > 0.
fn parse_smaps(text: &str) -> Vec<Region> {
    let mut out = Vec::new();
    let mut cur: Option<(Region, bool)> = None;
    let flush = |cur: &mut Option<(Region, bool)>, out: &mut Vec<Region>| {
        if let Some((r, true)) = cur.take() {
            if r.swap > 0 && r.end > r.start {
                out.push(r);
            }
        }
    };
    for line in text.lines() {
        if let Some(h) = parse_header(line) {
            flush(&mut cur, &mut out);
            cur = Some(h);
            continue;
        }
        let Some((r, ok)) = cur.as_mut() else { continue };
        if let Some(v) = line.strip_prefix("Swap:") {
            r.swap = v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0) * 1024;
        } else if let Some(v) = line.strip_prefix("VmFlags:") {
            if v.split_whitespace().any(|f| SKIP_FLAGS.contains(&f)) {
                *ok = false;
            }
        }
    }
    flush(&mut cur, &mut out);
    out
}

// ───────────────────────────── work planning ─────────────────────────────

/// One work item: one process_madvise call (split only if it exceeds IOV_MAX) in one process.
#[derive(Debug, Clone, PartialEq)]
struct Chunk {
    /// Index into the process list of the wake.
    proc_: usize,
    iov: Vec<IoVec>,
    /// Estimated swapped bytes covered (assumes swap is spread evenly within a region).
    est: u64,
}

fn ceil_div(a: u64, b: u64) -> u64 {
    a.div_ceil(b.max(1))
}

/// Splits regions into ~`chunk` bytes of swap each, largest first. A big region becomes several
/// address-range pieces; small regions of one process are grouped into a multi-iovec chunk.
fn plan(procs: &[Vec<Region>], chunk: u64, page: u64) -> Vec<Chunk> {
    let mut out = Vec::new();
    for (pi, regions) in procs.iter().enumerate() {
        let mut rs = regions.clone();
        rs.sort_by(|a, b| b.swap.cmp(&a.swap));
        let mut small = Chunk { proc_: pi, iov: Vec::new(), est: 0 };
        for r in rs {
            let len = r.end - r.start;
            if r.swap >= chunk {
                let n = ceil_div(r.swap, chunk);
                let piece = (ceil_div(ceil_div(len, n), page) * page).max(page);
                let mut s = r.start;
                while s < r.end {
                    let e = s.saturating_add(piece).min(r.end);
                    let est = (r.swap as u128 * (e - s) as u128 / len as u128) as u64;
                    out.push(Chunk { proc_: pi, iov: vec![IoVec { base: s as usize, len: (e - s) as usize }], est });
                    s = e;
                }
            } else {
                small.iov.push(IoVec { base: r.start as usize, len: len as usize });
                small.est += r.swap;
                if small.est >= chunk || small.iov.len() >= IOV_MAX {
                    out.push(std::mem::replace(&mut small, Chunk { proc_: pi, iov: Vec::new(), est: 0 }));
                }
            }
        }
        if !small.iov.is_empty() {
            out.push(small);
        }
    }
    out.sort_by(|a, b| b.est.cmp(&a.est)); // stable: equal pieces keep address order
    out
}

/// Keeps the largest chunks that fit into `budget` bytes.
fn cap(chunks: Vec<Chunk>, budget: u64) -> Vec<Chunk> {
    let mut sum = 0u64;
    chunks
        .into_iter()
        .filter(|c| {
            let fits = sum + c.est <= budget;
            if fits {
                sum += c.est;
            }
            fits
        })
        .collect()
}

// ───────────────────────────── execution ─────────────────────────────

#[derive(Debug, Default, PartialEq)]
struct Outcome {
    bytes: u64,
    /// iovecs the kernel refused (range unmapped meanwhile, ...).
    skipped: usize,
    gone: bool,
    fatal: Option<i32>,
    last_err: Option<i32>,
}

/// Advises one chunk: IOV_MAX batching, partial returns and per-iovec retry after an error.
fn advise_iovs(k: &dyn Kernel, fd: i32, iov: &[IoVec]) -> Outcome {
    let mut o = Outcome::default();
    // Returns false when the whole chunk must stop (process gone or fatal error).
    let verified = k.verified();
    let classify = |e: i32, o: &mut Outcome| -> bool {
        match classify(e, verified) {
            ErrClass::SkipProc => {
                o.gone = true;
                if e == EINVAL || e == EBADF {
                    o.last_err = Some(e);
                }
            }
            ErrClass::Fatal => o.fatal = Some(e),
            ErrClass::SkipRange => {
                o.last_err = Some(e);
                return true;
            }
        }
        false
    };
    for batch in iov.chunks(IOV_MAX) {
        let mut rest = batch;
        while !rest.is_empty() {
            match k.willneed(fd, rest) {
                Ok(n) => {
                    o.bytes += n as u64;
                    let total: usize = rest.iter().map(|v| v.len).sum();
                    if n >= total {
                        break;
                    }
                    // 6.6+: the kernel stopped at the first iovec it could not apply and reports
                    // the bytes before it. Skip that one and go on with the next.
                    let (mut left, mut i) = (n, 0);
                    while i < rest.len() && left >= rest[i].len {
                        left -= rest[i].len;
                        i += 1;
                    }
                    o.skipped += 1;
                    rest = &rest[(i + 1).min(rest.len())..];
                }
                Err(e) => {
                    if !classify(e, &mut o) {
                        return o;
                    }
                    if rest.len() == 1 {
                        o.skipped += 1;
                        break;
                    }
                    // 5.15–6.5 return only the error, not how far they got: redo one by one.
                    // Already advised ranges are cheap the second time (pages are in swap cache).
                    for v in rest {
                        match k.willneed(fd, std::slice::from_ref(v)) {
                            Ok(n) => o.bytes += n as u64,
                            Err(e) => {
                                if !classify(e, &mut o) {
                                    return o;
                                }
                                o.skipped += 1;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
    o
}

#[derive(Debug, Default)]
struct Exec {
    chunks: usize,
    /// Sum of `est` over the chunks that completed.
    est: u64,
    /// Address-range bytes the kernel accepted (not swap bytes).
    bytes: u64,
    skipped: usize,
    gone: usize,
    fatal: Option<i32>,
    last_err: Option<i32>,
    /// Why the valves stopped the wake early.
    stopped: Option<String>,
}

/// Runs the chunks on `threads` workers pulling from a shared queue (largest first). The calling
/// thread polls `valve` meanwhile; when it returns a reason, workers stop after their current chunk.
fn execute(
    k: &dyn Kernel,
    fds: &[Option<i32>],
    chunks: &[Chunk],
    threads: usize,
    mut valve: impl FnMut() -> Option<String>,
) -> Exec {
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let gone: Vec<AtomicBool> = fds.iter().map(|_| AtomicBool::new(false)).collect();
    let acc = Mutex::new(Exec::default());
    let n = threads.clamp(1, chunks.len().max(1));
    let mut stopped = None;
    let (tx, rx) = mpsc::channel::<()>();
    thread::scope(|s| {
        for _ in 0..n {
            let (next, stop, gone, acc, tx) = (&next, &stop, &gone, &acc, tx.clone());
            s.spawn(move || {
                let mut l = Exec::default();
                while !stop.load(Ordering::Relaxed) {
                    let Some(c) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) else { break };
                    let Some(fd) = fds.get(c.proc_).copied().flatten() else { continue };
                    if gone[c.proc_].load(Ordering::Relaxed) {
                        continue;
                    }
                    let o = advise_iovs(k, fd, &c.iov);
                    l.bytes += o.bytes;
                    l.skipped += o.skipped;
                    l.last_err = o.last_err.or(l.last_err);
                    if o.gone {
                        if !gone[c.proc_].swap(true, Ordering::Relaxed) {
                            l.gone += 1;
                        }
                    } else if o.fatal.is_none() {
                        l.chunks += 1;
                        l.est += c.est;
                    }
                    if let Some(e) = o.fatal {
                        l.fatal = Some(e);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                let mut a = acc.lock().unwrap_or_else(|p| p.into_inner());
                a.chunks += l.chunks;
                a.est += l.est;
                a.bytes += l.bytes;
                a.skipped += l.skipped;
                a.gone += l.gone;
                a.fatal = a.fatal.or(l.fatal);
                a.last_err = a.last_err.or(l.last_err);
                drop(a);
                let _ = tx.send(());
            });
        }
        drop(tx);
        let mut done = 0;
        while done < n {
            match rx.recv_timeout(VALVE_POLL) {
                Ok(()) => done += 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if stopped.is_none() {
                        if let Some(why) = valve() {
                            stopped = Some(why);
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    let mut e = acc.into_inner().unwrap_or_else(|p| p.into_inner());
    e.stopped = stopped;
    e
}

// ───────────────────────────── cgroups & memory ─────────────────────────────

/// UIDs with a user slice: `user.slice/user-UID.slice`.
fn uids(sys: &Sys) -> Vec<u32> {
    uids_in(&sys.cgroup)
}

/// `uids` for an explicit cgroup2 root (shared with the age helper).
pub(crate) fn uids_in(cgroup: &Path) -> Vec<u32> {
    let mut v: Vec<u32> = fs::read_dir(cgroup.join("user.slice"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.strip_prefix("user-")?.strip_suffix(".slice")?.parse().ok()
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort_unstable();
    v
}

/// Every app cgroup of every logged-in user:
/// `user.slice/user-UID.slice/user@UID.service/app.slice/*`; with `--profile server` the server
/// workloads instead (the same set the server daemon and the age helper use).
fn app_cgroups(sys: &Sys) -> Vec<PathBuf> {
    match &sys.server {
        Some(d) => workloads(&sys.cgroup, d, None),
        None => app_cgroups_in(&sys.cgroup),
    }
}

/// `app_cgroups` for an explicit cgroup2 root (shared with the age helper).
pub(crate) fn app_cgroups_in(cgroup: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for uid in uids_in(cgroup) {
        let slice = cgroup.join(format!("user.slice/user-{uid}.slice/user@{uid}.service/app.slice"));
        if let Ok(rd) = fs::read_dir(&slice) {
            let mut v: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect();
            v.sort();
            out.extend(v);
        }
    }
    out
}

fn name_of(cg: &Path) -> String {
    disp(&app_key(&cg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()))
}

/// Cumulative swap-in signals of one cgroup (memory.stat is hierarchical).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Counters {
    /// Anonymous pages read back that the kernel remembered evicting (our prefetch counts too).
    refault: u64,
    /// Faults that had to wait for a read (anon swap-in *or* file I/O); not raised by WILLNEED.
    majfault: u64,
    swap: u64,
}

fn counters(cg: &Path) -> Counters {
    let stat = read(&cg.join("memory.stat"));
    Counters {
        refault: kv(&stat, "workingset_refault_anon"),
        majfault: kv(&stat, "pgmajfault"),
        swap: read_u64(&cg.join("memory.swap.current")),
    }
}

/// Where a wake's memory stands: compared before/after to report what came back.
#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    swap: u64,
    swapcached: u64,
}

fn usage(cg: &Path) -> Usage {
    Usage {
        swap: read_u64(&cg.join("memory.swap.current")),
        swapcached: kv(&read(&cg.join("memory.stat")), "swapcached"),
    }
}

/// (MemTotal, MemAvailable) in bytes.
fn mem(sys: &Sys) -> (u64, u64) {
    let t = read(&sys.proc_.join("meminfo"));
    let g = |k: &str| -> u64 {
        t.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix(':'))
            .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (g("MemTotal"), g("MemAvailable"))
}

fn global_psi10(sys: &Sys) -> f64 {
    psi(&sys.proc_.join("pressure/memory")).some10
}

/// Smallest `limit − usage` over the cgroup and its ancestors (memory.high and memory.max);
/// None when nothing is limited. Prefetched pages are charged to the app, so overshooting its
/// memory.high (e.g. an `ultra-ram limit`) would just make the kernel reclaim them again.
fn headroom(sys: &Sys, cg: &Path) -> Option<u64> {
    let mut best: Option<u64> = None;
    let mut p = cg.to_path_buf();
    while p.starts_with(&sys.cgroup) && p != sys.cgroup {
        let cur = read_u64(&p.join("memory.current"));
        for f in ["memory.high", "memory.max"] {
            if let Ok(lim) = read(&p.join(f)).trim().parse::<u64>() {
                let h = lim.saturating_sub(cur);
                best = Some(best.map_or(h, |b| b.min(h)));
            }
        }
        if !p.pop() {
            break;
        }
    }
    best
}

/// How many bytes a wake may bring back: MemAvailable minus the floor and a margin, and at most
/// 80% of the cgroup's headroom under its limits.
fn budget(total: u64, avail: u64, floor_pct: u64, headroom: Option<u64>) -> u64 {
    let b = avail.saturating_sub(total / 100 * floor_pct + MARGIN);
    match headroom {
        Some(h) => b.min(h / 10 * 8),
        None => b,
    }
}

/// `0::/user.slice/...` (cgroup v2 line of /proc/PID/cgroup) → `user.slice/...`.
fn cgroup_rel(text: &str) -> Option<&str> {
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim().trim_start_matches('/');
    (!rel.is_empty() && !rel.split('/').any(|c| c == "..")).then_some(rel)
}

/// `0::/user.slice/...` from /proc/PID/cgroup → the cgroup directory, if it exists.
fn proc_cgroup(sys: &Sys, pid: u32) -> Option<PathBuf> {
    let p = sys.cgroup.join(cgroup_rel(&read(&sys.proc_.join(format!("{pid}/cgroup"))))?);
    p.is_dir().then_some(p)
}

/// Does PID live in `cg` or below it right now?
fn in_cgroup(sys: &Sys, pid: u32, cg: &Path) -> bool {
    cgroup_rel(&read(&sys.proc_.join(format!("{pid}/cgroup")))).is_some_and(|rel| sys.cgroup.join(rel).starts_with(cg))
}

// ───────────────────────────── markers for the daemon ─────────────────────────────

/// Cgroup directory name → marker file name (format in the module docs). None: no marker.
/// The age helper (src/age.rs) names its files the same way.
pub(crate) fn mark_name(dir: &str) -> Option<String> {
    let mut s: String = dir
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-') { c } else { '_' })
        .collect();
    if s.starts_with('.') {
        s.replace_range(..1, "_");
    }
    (!s.is_empty() && s.len() <= 200).then_some(s)
}

/// Writes `dir/<mark_name(cg_name)>` = `"<now> <refault>\n"` (see the module docs) through
/// `write_file_in` (no symlink is ever followed), then prunes markers older than MARK_TTL.
fn write_marker(dir: &Path, cg_name: &str, refault: u64, now: u64) -> Result<(), String> {
    let name = mark_name(cg_name).ok_or(format!("marker adı yararsızdır: {cg_name:?}"))?;
    write_file_in(dir, &name, &format!("{now} {refault}\n"))?;
    // Prune markers of long-gone cgroups (scope names are unique per launch).
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|a| a > MARK_TTL);
            if old && !e.file_name().to_string_lossy().starts_with('.') {
                let _ = fs::remove_file(e.path());
            }
        }
    }
    Ok(())
}

/// Atomically replaces `dir/name` (a name from `mark_name`) with `content`, 0644 in a 0755
/// directory created if missing. Never follows a symlink: the directory must be a real directory
/// not writable by group/others, the file is created with O_EXCL under a temporary name and renamed
/// over the target (rename replaces a link itself). Shared with the age helper.
pub(crate) fn write_file_in(dir: &Path, name: &str, content: &str) -> Result<(), String> {
    if fs::symlink_metadata(dir).is_err() {
        fs::DirBuilder::new()
            .mode(0o755)
            .create(dir)
            .and_then(|_| fs::set_permissions(dir, fs::Permissions::from_mode(0o755)))
            .map_err(|e| format!("{} yaradılmadı: {e}", dir.display()))?;
    }
    let m = fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !m.file_type().is_dir() || m.permissions().mode() & 0o022 != 0 {
        return Err(format!("{} kataloq deyil, simvolik linkdir və ya başqalarına yazıla biləndir — fayl yazılmadı", dir.display()));
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = fs::remove_file(&tmp);
    let r = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)
        .and_then(|mut f| {
            f.set_permissions(fs::Permissions::from_mode(0o644))?;
            f.write_all(content.as_bytes())
        })
        .and_then(|_| fs::rename(&tmp, dir.join(name)));
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{} yazılmadı: {e}", dir.join(name).display()));
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// After a wake that ran: record the refault counter it left behind. Failures only warn.
fn mark(sys: &Sys, cg: &Path) {
    let name = cg.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    if let Err(e) = write_marker(&sys.marks, &name, counters(cg).refault, unix_now()) {
        println!("! {e}");
    }
}

// ───────────────────────────── trigger state machine ─────────────────────────────

#[derive(Debug, PartialEq, Clone, Copy)]
enum Decision {
    /// First sample of this cgroup: nothing to compare against yet.
    Baseline,
    Quiet,
    Cooldown,
    /// Swapping in, but too little left in swap to be worth a wake.
    LowSwap,
    Fire { pages: u64 },
}

/// Fires when, since the previous sample, the cgroup refaulted ≥ trigger anonymous pages *and*
/// took at least one major fault — i.e. the app itself is waiting on swap-ins. The majfault
/// condition keeps our own WILLNEED swap-ins (which raise only the refault counter) from
/// re-triggering a wake; the cooldown covers the rest.
fn decide(prev: Option<&Counters>, cur: &Counters, now_ms: u64, until_ms: u64, o: &Opts) -> Decision {
    let Some(p) = prev else { return Decision::Baseline };
    let pages = cur.refault.saturating_sub(p.refault);
    if pages < o.trigger_pages || cur.majfault <= p.majfault {
        return Decision::Quiet;
    }
    if now_ms < until_ms {
        return Decision::Cooldown;
    }
    if cur.swap < o.min_swap_mb * MB {
        return Decision::LowSwap;
    }
    Decision::Fire { pages }
}

#[derive(Debug, Clone, Copy)]
struct Track {
    last: Counters,
    until: u64,
}

#[derive(Default)]
struct Tracker {
    cgs: HashMap<PathBuf, Track>,
}

impl Tracker {
    /// Feeds one round of samples; cgroups that disappeared are forgotten.
    fn step(&mut self, now_ms: u64, samples: Vec<(PathBuf, Counters)>, o: &Opts) -> Vec<(PathBuf, Decision)> {
        let mut seen = HashMap::with_capacity(samples.len());
        let mut out = Vec::with_capacity(samples.len());
        for (p, c) in samples {
            let prev = self.cgs.get(&p).copied();
            let until = prev.map_or(0, |t| t.until);
            let d = decide(prev.as_ref().map(|t| &t.last), &c, now_ms, until, o);
            seen.insert(p.clone(), Track { last: c, until });
            out.push((p, d));
        }
        self.cgs = seen;
        out
    }
    fn hold(&mut self, p: &Path, until_ms: u64) {
        if let Some(t) = self.cgs.get_mut(p) {
            t.until = until_ms;
        }
    }
}

// ───────────────────────────── arguments ─────────────────────────────

#[derive(Debug, PartialEq, Clone)]
struct Opts {
    /// 0 = number of CPUs minus one.
    threads: usize,
    interval_ms: u64,
    trigger_pages: u64,
    min_swap_mb: u64,
    cooldown_secs: u64,
    max_psi: f64,
    floor_pct: u64,
    verbose: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            threads: 0,
            interval_ms: 500,
            trigger_pages: 256,
            min_swap_mb: 16,
            cooldown_secs: 60,
            max_psi: 40.0,
            floor_pct: 10,
            verbose: false,
        }
    }
}

#[derive(Debug, PartialEq, Clone)]
enum Target {
    /// Lower-cased substring of the app name.
    Name(String),
    Pid(u32),
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Run(Opts),
    Once(Opts, Target),
    Help,
}

fn parse_args(args: &[String]) -> Result<Cmd, String> {
    let once = match args.first().map(|s| s.as_str()) {
        None | Some("help") | Some("--help") | Some("-h") => return Ok(Cmd::Help),
        Some("run") => false,
        Some("once") => true,
        Some(x) => return Err(format!("naməlum əmr: prefetch {x}")),
    };
    let mut o = Opts::default();
    let mut target = None;
    let mut it = args.iter().skip(1);
    fn num<'a>(flag: &str, it: &mut impl Iterator<Item = &'a String>, lo: u64, hi: u64) -> Result<u64, String> {
        let v = it.next().ok_or(format!("{flag} üçün dəyər verilməyib"))?;
        let n: u64 = v.parse().map_err(|_| format!("{flag}: rəqəm gözlənilir, '{v}' verilib"))?;
        if !(lo..=hi).contains(&n) {
            return Err(format!("{flag} {lo}..{hi} aralığında olmalıdır"));
        }
        Ok(n)
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--verbose" | "-v" => o.verbose = true,
            "--threads" => o.threads = num(a, &mut it, 1, MAX_THREADS)? as usize,
            "--interval-ms" => o.interval_ms = num(a, &mut it, 100, 60_000)?,
            "--trigger-pages" => o.trigger_pages = num(a, &mut it, 1, 1 << 30)?,
            "--min-swap-mb" => o.min_swap_mb = num(a, &mut it, 0, 1 << 20)?,
            "--cooldown-secs" => o.cooldown_secs = num(a, &mut it, 0, 86_400)?,
            "--max-psi" => o.max_psi = num(a, &mut it, 1, 100)? as f64,
            "--mem-floor-pct" => o.floor_pct = num(a, &mut it, 1, 90)?,
            "--pid" if once && target.is_none() => {
                target = Some(Target::Pid(num(a, &mut it, 1, u32::MAX as u64)? as u32));
            }
            x if once && target.is_none() && !x.starts_with('-') && !x.is_empty() => {
                target = Some(Target::Name(x.to_lowercase()));
            }
            x => return Err(format!("naməlum parametr: {x}")),
        }
    }
    if once {
        let t = target.ok_or("prefetch once: tətbiq adı və ya --pid PID lazımdır")?;
        Ok(Cmd::Once(o, t))
    } else {
        Ok(Cmd::Run(o))
    }
}

fn worker_count(o: &Opts, cpus: usize) -> usize {
    if o.threads > 0 {
        o.threads
    } else {
        cpus.saturating_sub(1).clamp(1, MAX_THREADS as usize)
    }
}

// ───────────────────────────── wake ─────────────────────────────

struct Wake {
    name: String,
    cg: Option<PathBuf>,
    pids: Vec<u32>,
}

#[derive(Debug, Default)]
struct Report {
    name: String,
    skipped: Option<String>,
    procs: usize,
    threads: usize,
    regions: usize,
    /// Estimated swapped bytes selected for this wake.
    planned: u64,
    /// Estimated swapped bytes left out because of the memory budget.
    cut: u64,
    exec: Exec,
    /// memory.swap.current before − after (pages the app touched again and freed from swap).
    swap_drop: i64,
    /// memory.stat swapcached after − before (pages we read in that the app has not touched yet).
    cache_rise: i64,
    ms: u128,
}

impl Report {
    fn skip(name: &str, why: String) -> Report {
        Report { name: name.into(), skipped: Some(why), ..Default::default() }
    }
    fn line(&self, trigger: Option<u64>) -> String {
        let m = |b: i64| b / MB as i64;
        if let Some(why) = &self.skipped {
            return format!("· skip {}: {why}", self.name);
        }
        let mut s = format!(
            "⚡ wake {}: istənilən {}M, swap↓ {}M, swapcache↑ {}M (RAM-a qayıdan ≈{}M), {} proses, {} axın, {} ms",
            self.name,
            self.planned / MB,
            m(self.swap_drop),
            m(self.cache_rise),
            m((self.swap_drop + self.cache_rise).max(0)),
            self.procs,
            self.threads,
            self.ms
        );
        if let Some(p) = trigger {
            s += &format!(", tetik {p} səhifə");
        }
        if self.cut > 0 {
            s += &format!(", RAM büdcəsinə görə {}M buraxıldı", self.cut / MB);
        }
        if let Some(why) = &self.exec.stopped {
            s += &format!(", erkən dayandı ({why}; {}M edildi)", self.exec.est / MB);
        }
        if self.exec.gone > 0 {
            s += &format!(", {} proses çıxdı və ya əlçatmazdır", self.exec.gone);
        }
        if self.exec.skipped > 0 {
            s += &format!(", {} aralıq rədd edildi (son errno {})", self.exec.skipped, self.exec.last_err.unwrap_or(0));
        }
        s
    }
}

/// Swaps in one app (all `pids`, measured on `cg`). Err only for errors that end the service.
fn wake(sys: &Sys, k: &dyn Kernel, o: &Opts, w: &Wake, threads: usize) -> Result<Report, String> {
    let t0 = Instant::now();
    let p10 = global_psi10(sys);
    if p10 > o.max_psi {
        return Ok(Report::skip(&w.name, format!("qlobal yaddaş PSI {p10:.1}% > {}%", o.max_psi)));
    }
    let (total, avail) = mem(sys);
    let b = budget(total, avail, o.floor_pct, w.cg.as_deref().and_then(|c| headroom(sys, c)));
    if b < (o.min_swap_mb * MB).max(CHUNK) {
        return Ok(Report::skip(
            &w.name,
            format!("RAM azdır: MemAvailable {}M, büdcə {}M (döşəmə {}% + {}M)", avail / MB, b / MB, o.floor_pct, MARGIN / MB),
        ));
    }
    let before = w.cg.as_deref().map(usage);

    // pidfd first, smaps second: a PID reused in between cannot receive another process's ranges
    // (the pidfd of the dead process fails with ESRCH instead).
    let mut fds = FdSet::open(k, &w.pids)?;
    // PID reuse between pids_in and pidfd_open: the pidfd pins whichever process holds the PID
    // now, so keep it only if that process still lives in the cgroup being woken.
    if let Some(cg) = w.cg.as_deref() {
        for (i, pid) in w.pids.iter().enumerate() {
            if fds.fds[i].is_some() && !in_cgroup(sys, *pid, cg) {
                fds.drop_at(i);
            }
        }
    }
    let regions: Vec<Vec<Region>> = w
        .pids
        .iter()
        .zip(&fds.fds)
        .map(|(pid, fd)| match fd {
            Some(_) => parse_smaps(&read(&sys.proc_.join(format!("{pid}/smaps")))),
            None => Vec::new(),
        })
        .collect();
    let all = plan(&regions, CHUNK, page_size());
    let wanted: u64 = all.iter().map(|c| c.est).sum();
    let chunks = cap(all, b);
    let mut r = Report {
        name: w.name.clone(),
        procs: fds.fds.iter().flatten().count(),
        threads,
        regions: regions.iter().map(Vec::len).sum(),
        planned: chunks.iter().map(|c| c.est).sum(),
        ..Default::default()
    };
    r.cut = wanted - r.planned;
    if chunks.is_empty() {
        r.skipped = Some(if wanted == 0 {
            "swap-da anonim səhifə tapılmadı (smaps)".into()
        } else {
            format!("RAM büdcəsi ({}M) ən kiçik hissəyə də çatmır", b / MB)
        });
        return Ok(r);
    }
    if o.verbose {
        println!(
            "  {}: {} proses, {} region, {} hissə, plan {}M / büdcə {}M",
            w.name,
            r.procs,
            r.regions,
            chunks.len(),
            r.planned / MB,
            b / MB
        );
    }
    let floor = total / 100 * o.floor_pct;
    r.exec = execute(k, &fds.fds, &chunks, threads.min(chunks.len()), || {
        let (_, a) = mem(sys);
        if a < floor {
            return Some(format!("MemAvailable {}M < {}M", a / MB, floor / MB));
        }
        let p = global_psi10(sys);
        (p > o.max_psi).then(|| format!("PSI {p:.1}% > {}%", o.max_psi))
    });
    r.threads = threads.min(chunks.len());
    r.ms = t0.elapsed().as_millis();
    if let Some(e) = r.exec.fatal {
        return Err(fatal_msg(e));
    }
    if let (Some(b4), Some(cg)) = (before, w.cg.as_deref()) {
        let now = usage(cg);
        r.swap_drop = b4.swap as i64 - now.swap as i64;
        r.cache_rise = now.swapcached as i64 - b4.swapcached as i64;
    }
    Ok(r)
}

// ───────────────────────────── modes ─────────────────────────────

fn service(sys: &Sys, k: &dyn Kernel, o: &Opts) -> i32 {
    let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let threads = worker_count(o, cpus);
    println!(
        "ultra-ram prefetch: işə düşdü — {threads} axın, hər {} ms, tetik ≥{} səhifə, min swap {}M, fasilə {} s, PSI həddi {}%, RAM döşəməsi {}%",
        o.interval_ms, o.trigger_pages, o.min_swap_mb, o.cooldown_secs, o.max_psi, o.floor_pct
    );
    if let Some(d) = &sys.server {
        println!("server profile: watching the workloads below {} (depth {})", d.roots.join(", "), d.depth);
    }
    let start = Instant::now();
    let ms = || start.elapsed().as_millis() as u64;
    let mut tr = Tracker::default();
    let mut known_users: Vec<u32> = Vec::new();
    loop {
        let tick = Instant::now();
        if o.verbose && sys.server.is_none() {
            let u = uids(sys);
            if u != known_users {
                println!("  istifadəçilər: {u:?}");
                known_users = u;
            }
        }
        let samples: Vec<(PathBuf, Counters)> = app_cgroups(sys).into_iter().map(|p| {
            let c = counters(&p);
            (p, c)
        }).collect();
        let mut fire = Vec::new();
        for (p, d) in tr.step(ms(), samples, o) {
            match d {
                Decision::Fire { pages } => fire.push((p, pages)),
                Decision::Cooldown | Decision::LowSwap if o.verbose => println!("  {}: {d:?}", name_of(&p)),
                _ => {}
            }
        }
        fire.sort_by(|a, b| b.1.cmp(&a.1));
        for (p, pages) in fire {
            let mut pids = Vec::new();
            pids_in(&p, &mut pids);
            let w = Wake { name: name_of(&p), cg: Some(p.clone()), pids };
            match wake(sys, k, o, &w, threads) {
                Ok(r) => {
                    println!("{}", r.line(Some(pages)));
                    if r.skipped.is_none() {
                        mark(sys, &p);
                    }
                    let hold = if r.skipped.is_some() { SKIP_RETRY_MS } else { o.cooldown_secs * 1000 };
                    tr.hold(&p, ms() + hold);
                }
                Err(e) => {
                    println!("✗ {e}");
                    return EXIT_UNSUPPORTED;
                }
            }
        }
        if let Some(rest) = Duration::from_millis(o.interval_ms).checked_sub(tick.elapsed()) {
            thread::sleep(rest);
        }
    }
}

fn once(sys: &Sys, k: &dyn Kernel, o: &Opts, t: &Target) -> i32 {
    let cpus = thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let threads = worker_count(o, cpus);
    let wakes: Vec<Wake> = match t {
        Target::Pid(pid) => {
            if !sys.proc_.join(pid.to_string()).is_dir() {
                println!("✗ PID {pid} yoxdur");
                return 1;
            }
            let comm = read(&sys.proc_.join(format!("{pid}/comm"))).trim().to_string();
            vec![Wake { name: format!("{comm} (PID {pid})"), cg: proc_cgroup(sys, *pid), pids: vec![*pid] }]
        }
        Target::Name(s) => {
            let cgs = app_cgroups(sys);
            let hits: Vec<Wake> = cgs
                .iter()
                .filter(|p| {
                    let dir = p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                    app_key(&dir).contains(s.as_str()) || dir.contains(s.as_str())
                })
                .map(|p| {
                    let mut pids = Vec::new();
                    pids_in(p, &mut pids);
                    Wake { name: name_of(p), cg: Some(p.clone()), pids }
                })
                .collect();
            if hits.is_empty() {
                let mut names: Vec<String> = cgs.iter().map(|p| name_of(p)).collect();
                names.sort();
                names.dedup();
                println!("✗ '{s}' adlı tətbiq tapılmadı. Mövcud: {}", names.join(", "));
                return 1;
            }
            hits
        }
    };
    for w in &wakes {
        match wake(sys, k, o, w, threads) {
            Ok(r) => {
                println!("{}", r.line(None));
                if let (None, Some(cg)) = (&r.skipped, w.cg.as_deref()) {
                    mark(sys, cg);
                }
            }
            Err(e) => {
                println!("✗ {e}");
                return EXIT_UNSUPPORTED;
            }
        }
    }
    0
}

// ───────────────────────────── entry point ─────────────────────────────

fn help() {
    println!(
        "ultra-ram prefetch — [root, istəyə bağlı] tətbiqə qayıdanda onun zram-dakı yaddaşını
qabaqcadan, paralel olaraq RAM-a qaytarır (process_madvise MADV_WILLNEED, Linux ≥ 5.15)

  run                   Xidmət rejimi: tətbiq swap-dan oxumağa başlayanda onu \"oyandır\"
      --threads N       İşçi axın sayı (standart: CPU sayı − 1)
      --interval-ms M   Ölçmə intervalı (standart 500)
      --trigger-pages P Bir intervalda ən az P səhifə geri oxunsa işə düş (standart 256)
      --min-swap-mb S   Tətbiqin swap-da ən az S MB-ı olmalıdır (standart 16)
      --cooldown-secs C Eyni tətbiqi C saniyə yenidən oyandırma (standart 60)
      --max-psi X       Qlobal yaddaş PSI some avg10 X%-dən yüksəkdirsə dayan (standart 40)
      --mem-floor-pct F MemAvailable RAM-ın F%-indən aşağı düşməsin (standart 10)
      --verbose         Ətraflı jurnal
  once <ad>             Adında <ad> olan tətbiqi dərhal oyandır (test və ya qısayol üçün)
  once --pid PID        Yalnız bu prosesi oyandır

Jurnal: hər oyandırma üçün bir '⚡ wake' sətri (istənilən MB, swap↓, swapcache↑, müddət)."
    );
}

fn is_root(sys: &Sys) -> bool {
    read(&sys.proc_.join("self/status"))
        .lines()
        .find_map(|l| l.strip_prefix("Uid:"))
        .and_then(|r| r.split_whitespace().nth(1)?.parse::<u32>().ok())
        == Some(0)
}

/// `ultra-ram prefetch ...`; returns the process exit code.
pub fn run(args: &[String], server: Option<Discovery>) -> i32 {
    let cmd = match parse_args(args) {
        Ok(c) => c,
        Err(e) => {
            println!("✗ {e}\n");
            help();
            return 2;
        }
    };
    let sys = Sys { server, ..Sys::real() };
    if cmd == Cmd::Help {
        help();
        return 0;
    }
    if !is_root(&sys) {
        println!("✗ Bu əmr root tələb edir: sudo ultra-ram prefetch {}", args.join(" "));
        return 1;
    }
    if NR.is_none() {
        println!("✗ prefetch yalnız x86_64 və aarch64 üçün qurulub");
        return EXIT_UNSUPPORTED;
    }
    if !sys.cgroup.join("cgroup.controllers").exists() {
        println!("✗ cgroup v2 lazımdır ({} cgroup2 deyil)", sys.cgroup.display());
        return EXIT_UNSUPPORTED;
    }
    let verified = match probe(&Real { verified: false }) {
        Ok(None) => true,
        Ok(Some(w)) => {
            println!("! {w}");
            false
        }
        Err(e) => {
            println!("✗ {e}");
            return EXIT_UNSUPPORTED;
        }
    };
    let k = Real { verified };
    match cmd {
        Cmd::Run(o) => service(&sys, &k, &o),
        Cmd::Once(o, t) => once(&sys, &k, &o, &t),
        Cmd::Help => 0,
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    const SMAPS: &str = "\
55d0c0000000-55d0c0021000 r--p 00000000 08:02 1234                       /usr/bin/foo
Size:                132 kB
Swap:                 64 kB
VmFlags: rd mr mw me dw sd
55d0c1000000-55d0c2000000 rw-p 00000000 00:00 0                          [heap]
Size:              16384 kB
Rss:                1024 kB
Swap:              10240 kB
SwapPss:           10240 kB
VmFlags: rd wr mr mw me ac sd
7f0000000000-7f0000400000 rw-p 00000000 00:00 0
Size:               4096 kB
Swap:               2048 kB
VmFlags: rd wr mr mw me ac sd
7f0000400000-7f0000800000 rw-s 00000000 00:01 77                         /dev/zero (deleted)
Swap:               1024 kB
VmFlags: rd wr sh mr mw me ms sd
7f0000800000-7f0000c00000 rw-p 00000000 00:00 0                          [anon:v8 heap]
Swap:                512 kB
VmFlags: rd wr mr mw me ac sd
7f0000c00000-7f0000d00000 rw-p 00000000 00:00 0
Swap:                256 kB
VmFlags: rd wr mr mw me ac dd sd
7f0000d00000-7f0000e00000 rw-p 00000000 00:00 0
Swap:                256 kB
VmFlags: rd wr mr mw me de ht sd
7f0000e00000-7f0000f00000 rw-p 00000000 00:00 0
Swap:                  0 kB
VmFlags: rd wr mr mw me ac sd
7f0000f00000-7f0001000000 rw-p 00000000 00:00 0                          [anon_shmem:x]
Swap:                128 kB
7f0001000000-7f0001100000 rw-p 00001000 08:02 999                        /home/u/my file.dat
Swap:                128 kB
VmFlags: rd wr mr mw me ac
7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0                          [stack]
Swap:                 12 kB
VmFlags: rd wr mr mw me gd ac
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
Swap:                  4 kB
7f0002000000-7f0002010000 rw-p 00000000 00:00 0
Swap:                 64 kB
";

    #[test]
    fn smaps_selects_private_anon_with_swap() {
        let r = parse_smaps(SMAPS);
        let starts: Vec<u64> = r.iter().map(|x| x.start).collect();
        assert_eq!(
            starts,
            vec![0x55d0c1000000, 0x7f0000000000, 0x7f0000800000, 0x7ffd00000000, 0x7f0002000000],
            "heap, unnamed, [anon:], [stack], last region without VmFlags"
        );
        assert_eq!(r[0], Region { start: 0x55d0c1000000, end: 0x55d0c2000000, swap: 10240 * 1024 });
        assert_eq!(r[1].swap, 2048 * 1024);
        assert_eq!(r[4].swap, 64 * 1024);
        assert!(parse_smaps("").is_empty());
        assert!(parse_smaps("garbage\nSwap: 5 kB\n").is_empty());
        for f in ["io", "pf", "sh", "ht", "dd"] {
            let t = format!("1000-2000 rw-p 00000000 00:00 0\nSwap: 4 kB\nVmFlags: rd wr {f}\n");
            assert!(parse_smaps(&t).is_empty(), "flag {f}");
        }
        assert!(parse_smaps("1000-2000 r--p 00000000 00:00 0 [vdso]\nSwap: 4 kB\n").is_empty());
        assert_eq!(parse_smaps("1000-2000 rw-p 00000000 00:00 0 [stack:123]\nSwap: 4 kB\n").len(), 1);
    }

    fn reg(start: u64, len: u64, swap: u64) -> Region {
        Region { start, end: start + len, swap }
    }

    #[test]
    fn plan_splits_groups_and_sorts() {
        let page = 4096;
        // 40 MB region with 10 MB swapped → 3 pieces covering it exactly; small ones grouped.
        let p0 = vec![reg(0x1000_0000, 40 * MB, 10 * MB), reg(0x9000_0000, MB, MB), reg(0xa000_0000, MB, 2 * MB)];
        let p1 = vec![reg(0x2000_0000, 8 * MB, 8 * MB)];
        let c = plan(&[p0, p1], CHUNK, page);
        let big: Vec<&Chunk> = c.iter().filter(|x| x.proc_ == 0 && x.iov.len() == 1 && x.iov[0].base >= 0x1000_0000 && x.iov[0].base < 0x9000_0000).collect();
        assert_eq!(big.len(), 3);
        assert_eq!(big.iter().map(|x| x.iov[0].len as u64).sum::<u64>(), 40 * MB);
        assert_eq!(big[0].iov[0].base, 0x1000_0000);
        for w in big.windows(2) {
            assert_eq!(w[0].iov[0].base + w[0].iov[0].len, w[1].iov[0].base, "contiguous");
        }
        assert!(big.iter().all(|x| x.iov[0].len as u64 % page == 0 || x.iov[0].base as u64 + x.iov[0].len as u64 == 0x1000_0000 + 40 * MB));
        let small: Vec<&Chunk> = c.iter().filter(|x| x.iov.len() == 2).collect();
        assert_eq!(small.len(), 1);
        assert_eq!(small[0].est, 3 * MB);
        assert_eq!(small[0].iov[0].base, 0xa000_0000, "largest swap first inside a group");
        let p1c: Vec<&Chunk> = c.iter().filter(|x| x.proc_ == 1).collect();
        assert_eq!(p1c.len(), 2);
        assert!(p1c.iter().all(|x| x.est == 4 * MB));
        assert!(c.windows(2).all(|w| w[0].est >= w[1].est), "sorted largest first");
        let total: u64 = c.iter().map(|x| x.est).sum();
        assert!(total <= 21 * MB && total >= 21 * MB - 4, "estimates add up: {total}");

        // Many tiny regions: grouped, never more than IOV_MAX per chunk.
        let tiny: Vec<Region> = (0..3000u64).map(|i| reg(i * 2 * page, page, 1024)).collect();
        let c = plan(&[tiny], CHUNK, page);
        assert!(c.iter().all(|x| x.iov.len() <= IOV_MAX));
        assert_eq!(c.iter().map(|x| x.iov.len()).sum::<usize>(), 3000);

        // Budget keeps the largest chunks that fit.
        let c = plan(&[vec![reg(0, 16 * MB, 16 * MB), reg(1 << 32, MB, MB)]], CHUNK, page);
        let kept = cap(c.clone(), 9 * MB);
        assert_eq!(kept.iter().map(|x| x.est).sum::<u64>(), 9 * MB);
        assert_eq!(kept.len(), 3);
        assert!(cap(c, 0).is_empty());
    }

    /// Fake syscalls: records calls, scripted results per call.
    struct FakeK {
        calls: Mutex<Vec<(i32, Vec<IoVec>)>>,
        closed: Mutex<Vec<i32>>,
        dead: Vec<u32>,
        verified: bool,
        script: Box<dyn Fn(i32, &[IoVec]) -> Result<usize, i32> + Sync>,
    }

    impl FakeK {
        fn ok() -> FakeK {
            FakeK::with(|_, iov| Ok(iov.iter().map(|v| v.len).sum()))
        }
        fn with(f: impl Fn(i32, &[IoVec]) -> Result<usize, i32> + Sync + 'static) -> FakeK {
            FakeK {
                calls: Mutex::new(Vec::new()),
                closed: Mutex::new(Vec::new()),
                dead: Vec::new(),
                verified: false,
                script: Box::new(f),
            }
        }
        fn advised(&self) -> Vec<IoVec> {
            let mut v: Vec<IoVec> = self.calls.lock().unwrap().iter().flat_map(|c| c.1.clone()).collect();
            v.sort_by_key(|x| x.base);
            v
        }
    }

    impl Kernel for FakeK {
        fn pidfd_open(&self, pid: u32) -> Result<i32, i32> {
            if self.dead.contains(&pid) {
                Err(ESRCH)
            } else {
                Ok(pid as i32 + 1000)
            }
        }
        fn close(&self, fd: i32) {
            self.closed.lock().unwrap().push(fd);
        }
        fn willneed(&self, fd: i32, iov: &[IoVec]) -> Result<usize, i32> {
            assert!(iov.len() <= IOV_MAX);
            self.calls.lock().unwrap().push((fd, iov.to_vec()));
            (self.script)(fd, iov)
        }
        fn verified(&self) -> bool {
            self.verified
        }
    }

    fn iov(base: usize, len: usize) -> IoVec {
        IoVec { base, len }
    }

    #[test]
    fn advise_batching_partial_and_errors() {
        // IOV_MAX batching.
        let k = FakeK::ok();
        let v: Vec<IoVec> = (0..2500).map(|i| iov(i * 8192, 4096)).collect();
        let o = advise_iovs(&k, 5, &v);
        assert_eq!(k.calls.lock().unwrap().iter().map(|c| c.1.len()).collect::<Vec<_>>(), vec![1024, 1024, 452]);
        assert_eq!(o, Outcome { bytes: 2500 * 4096, ..Default::default() });

        // 6.6+ partial return: second iovec unmapped → skipped, third still advised.
        let k = FakeK::with(|_, iov| if iov[0].base == 0 { Ok(4096) } else { Ok(iov.iter().map(|v| v.len).sum()) });
        let o = advise_iovs(&k, 5, &[iov(0, 4096), iov(8192, 4096), iov(16384, 4096)]);
        assert_eq!(o.skipped, 1);
        assert_eq!(o.bytes, 8192);
        assert_eq!(k.calls.lock().unwrap().last().unwrap().1, vec![iov(16384, 4096)]);

        // 5.15 style: whole call fails with ENOMEM → retried one by one, bad one skipped.
        let k = FakeK::with(|_, iov| if iov.len() > 1 || iov[0].base == 8192 { Err(12) } else { Ok(iov[0].len) });
        let o = advise_iovs(&k, 5, &[iov(0, 4096), iov(8192, 4096), iov(16384, 4096)]);
        assert_eq!((o.bytes, o.skipped, o.last_err, o.gone), (8192, 1, Some(12), false));

        // Process exited.
        let k = FakeK::with(|_, _| Err(ESRCH));
        let o = advise_iovs(&k, 5, &[iov(0, 4096), iov(8192, 4096)]);
        assert!(o.gone);
        assert_eq!(k.calls.lock().unwrap().len(), 1);

        // Not permitted → fatal.
        let k = FakeK::with(|_, _| Err(EPERM));
        assert_eq!(advise_iovs(&k, 5, &[iov(0, 4096)]).fatal, Some(EPERM));

        // EINVAL before the probe confirmed support: unsupported kernel → fatal.
        let k = FakeK::with(|_, _| Err(EINVAL));
        assert_eq!(advise_iovs(&k, 5, &[iov(0, 4096)]).fatal, Some(EINVAL));
        // After it: only this process is skipped; the service lives on.
        for e in [EINVAL, EBADF] {
            let mut k = FakeK::with(move |_, _| Err(e));
            k.verified = true;
            let o = advise_iovs(&k, 5, &[iov(0, 4096), iov(8192, 4096)]);
            assert_eq!((o.fatal, o.gone, o.last_err), (None, true, Some(e)));
            assert_eq!(k.calls.lock().unwrap().len(), 1);
        }
        let mut k = FakeK::with(|_, _| Err(EPERM));
        k.verified = true;
        assert_eq!(advise_iovs(&k, 5, &[iov(0, 4096)]).fatal, Some(EPERM), "EPERM stays fatal");
    }

    #[test]
    fn pids_in_depth_is_capped() {
        let f = Fake::new("deep");
        let mut d = f.app(1000, "app-deep.scope");
        let top = d.clone();
        for i in 0..40u32 {
            fs::write(d.join("cgroup.procs"), format!("{}\n", i + 1)).unwrap();
            d = d.join("x");
            fs::create_dir_all(&d).unwrap();
        }
        let mut pids = Vec::new();
        pids_in(&top, &mut pids);
        // The top level plus 32 levels below it.
        assert_eq!(pids, (1..=33).collect::<Vec<u32>>());
    }

    #[test]
    fn errno_classes() {
        for v in [false, true] {
            assert_eq!(classify(ESRCH, v), ErrClass::SkipProc);
            assert_eq!(classify(EACCES, v), ErrClass::SkipProc);
            assert_eq!(classify(EPERM, v), ErrClass::Fatal);
            assert_eq!(classify(ENOSYS, v), ErrClass::Fatal);
            assert_eq!(classify(12, v), ErrClass::SkipRange); // ENOMEM
        }
        assert_eq!(classify(EINVAL, false), ErrClass::Fatal);
        assert_eq!(classify(EBADF, false), ErrClass::Fatal);
        assert_eq!(classify(EINVAL, true), ErrClass::SkipProc);
        assert_eq!(classify(EBADF, true), ErrClass::SkipProc);
        assert_eq!(cgroup_rel("1:name=systemd:/x\n0::/user.slice/a.scope\n"), Some("user.slice/a.scope"));
        assert_eq!(cgroup_rel("0::/\n"), None);
        assert_eq!(cgroup_rel("0::/a/../b\n"), None);
    }

    #[test]
    fn marker_names_and_files() {
        assert_eq!(mark_name("app-firefox-1234.scope").as_deref(), Some("app-firefox-1234.scope"));
        assert_eq!(mark_name("app-com.google.Chrome@abc.service").as_deref(), Some("app-com.google.Chrome@abc.service"));
        assert_eq!(mark_name("a b/c\\x2dd\nä").as_deref(), Some("a_b_c_x2dd__"));
        assert_eq!(mark_name("..").as_deref(), Some("_."));
        assert_eq!(mark_name(".hidden").as_deref(), Some("_hidden"));
        assert_eq!(mark_name(""), None);
        assert_eq!(mark_name(&"x".repeat(201)), None);

        let f = Fake::new("mark");
        fs::create_dir_all(f.root.join("run")).unwrap();
        let dir = f.sys.marks.clone();
        write_marker(&dir, "app-x.scope", 777, 1_700_000_000).unwrap();
        assert_eq!(fs::read_to_string(dir.join("app-x.scope")).unwrap(), "1700000000 777\n");
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(fs::metadata(dir.join("app-x.scope")).unwrap().permissions().mode() & 0o777, 0o644);
        write_marker(&dir, "app-x.scope", 778, 1_700_000_001).unwrap();
        assert_eq!(fs::read_to_string(dir.join("app-x.scope")).unwrap(), "1700000001 778\n");

        // A symlink at the target is replaced, never written through.
        let victim = f.root.join("victim");
        fs::write(&victim, "keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("app-y.scope")).unwrap();
        write_marker(&dir, "app-y.scope", 1, 2).unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert!(!fs::symlink_metadata(dir.join("app-y.scope")).unwrap().file_type().is_symlink());

        // A symlinked marker directory is refused.
        let real_dir = f.root.join("elsewhere");
        fs::create_dir_all(&real_dir).unwrap();
        let link = f.root.join("run/linked");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();
        assert!(write_marker(&link, "app-x.scope", 1, 2).is_err());
        assert!(fs::read_dir(&real_dir).unwrap().next().is_none());

        // mark(): refault counter of the cgroup right now.
        let cg = f.app(1000, "app-z.scope");
        fs::write(cg.join("memory.stat"), "workingset_refault_anon 4242\n").unwrap();
        mark(&f.sys, &cg);
        let t = fs::read_to_string(dir.join("app-z.scope")).unwrap();
        assert!(t.ends_with(" 4242\n"), "{t}");
    }

    #[test]
    fn execute_spreads_work_once_each() {
        let page = 4096;
        let procs: Vec<Vec<Region>> = (0..3u64).map(|p| vec![reg((p + 1) << 32, 64 * MB, 64 * MB)]).collect();
        let chunks = plan(&procs, CHUNK, page);
        assert_eq!(chunks.len(), 48);
        let k = FakeK::ok();
        let fds = vec![Some(1001), Some(1002), Some(1003)];
        let e = execute(&k, &fds, &chunks, 4, || None);
        assert_eq!(e.chunks, 48);
        assert_eq!(e.est, 192 * MB);
        let adv = k.advised();
        assert_eq!(adv.len(), 48, "each chunk exactly once");
        let mut want: Vec<IoVec> = chunks.iter().flat_map(|c| c.iov.clone()).collect();
        want.sort_by_key(|x| x.base);
        assert_eq!(adv, want);
        for c in k.calls.lock().unwrap().iter() {
            let pi = (c.1[0].base as u64 >> 32) as i32;
            assert_eq!(c.0, 1000 + pi, "chunk goes to its own process");
        }

        // Process 2 exits: its remaining chunks are skipped, the others still run.
        let k = FakeK::with(|fd, iov| if fd == 1002 { Err(ESRCH) } else { Ok(iov[0].len) });
        let e = execute(&k, &fds, &chunks, 2, || None);
        assert_eq!(e.gone, 1);
        assert_eq!(e.chunks, 32);
        // Each worker may have pulled one chunk of it before seeing the exit.
        let tried = k.calls.lock().unwrap().iter().filter(|c| c.0 == 1002).count();
        assert!((1..=2).contains(&tried), "{tried}");

        // A process whose pidfd could not be opened is never touched.
        let k = FakeK::ok();
        let e = execute(&k, &[Some(1001), None, Some(1003)], &chunks, 3, || None);
        assert_eq!(e.chunks, 32);

        // Fatal error stops everything.
        let k = FakeK::with(|_, _| Err(ENOSYS));
        let e = execute(&k, &fds, &chunks, 3, || None);
        assert_eq!(e.fatal, Some(ENOSYS));
        assert!(k.calls.lock().unwrap().len() <= 3);

        // The valve stops workers early.
        let n = AtomicU64::new(0);
        let k = FakeK::with(move |_, iov| {
            thread::sleep(Duration::from_millis(30));
            Ok(iov[0].len)
        });
        let e = execute(&k, &fds, &chunks, 1, || {
            (n.fetch_add(1, Ordering::Relaxed) >= 1).then(|| "test".to_string())
        });
        assert_eq!(e.stopped.as_deref(), Some("test"));
        assert!(e.chunks < 48, "stopped after {} chunks", e.chunks);
    }

    #[test]
    fn trigger_and_cooldown() {
        let o = Opts::default();
        let c = |refault, majfault, swap_mb| Counters { refault, majfault, swap: swap_mb * MB };
        assert_eq!(decide(None, &c(0, 0, 100), 0, 0, &o), Decision::Baseline);
        assert_eq!(decide(Some(&c(0, 0, 100)), &c(255, 10, 100), 0, 0, &o), Decision::Quiet);
        assert_eq!(decide(Some(&c(0, 0, 100)), &c(256, 1, 100), 0, 0, &o), Decision::Fire { pages: 256 });
        // Refaults without any major fault: our own prefetch or readahead, not the app.
        assert_eq!(decide(Some(&c(0, 5, 100)), &c(5000, 5, 100), 0, 0, &o), Decision::Quiet);
        assert_eq!(decide(Some(&c(0, 0, 100)), &c(500, 3, 100), 999, 1000, &o), Decision::Cooldown);
        assert_eq!(decide(Some(&c(0, 0, 100)), &c(500, 3, 100), 1000, 1000, &o), Decision::Fire { pages: 500 });
        assert_eq!(decide(Some(&c(0, 0, 100)), &c(500, 3, 15), 0, 0, &o), Decision::LowSwap);
        // Counter reset (cgroup recreated under the same name): no bogus fire.
        assert_eq!(decide(Some(&c(9000, 50, 100)), &c(10, 1, 100), 0, 0, &o), Decision::Quiet);

        let a = PathBuf::from("/cg/a");
        let b = PathBuf::from("/cg/b");
        let mut t = Tracker::default();
        let d = t.step(0, vec![(a.clone(), c(0, 0, 100)), (b.clone(), c(0, 0, 100))], &o);
        assert!(d.iter().all(|x| x.1 == Decision::Baseline));
        let d = t.step(500, vec![(a.clone(), c(1000, 9, 100)), (b.clone(), c(10, 0, 100))], &o);
        assert_eq!(d, vec![(a.clone(), Decision::Fire { pages: 1000 }), (b.clone(), Decision::Quiet)]);
        t.hold(&a, 500 + 60_000);
        // Our own swap-ins raise the counter again: suppressed by the cooldown.
        let d = t.step(1000, vec![(a.clone(), c(50_000, 20, 100))], &o);
        assert_eq!(d, vec![(a.clone(), Decision::Cooldown)]);
        assert!(!t.cgs.contains_key(&b), "vanished cgroup forgotten");
        let d = t.step(1500, vec![(a.clone(), c(50_100, 21, 100))], &o);
        assert_eq!(d[0].1, Decision::Quiet, "baseline moved on during cooldown");
        let d = t.step(61_000, vec![(a.clone(), c(51_000, 30, 100))], &o);
        assert_eq!(d[0].1, Decision::Fire { pages: 900 });
        // b comes back: new baseline, not a fire from stale counters.
        let d = t.step(61_500, vec![(b.clone(), c(99_999, 99, 100))], &o);
        assert_eq!(d[0].1, Decision::Baseline);
    }

    #[test]
    fn args() {
        assert_eq!(parse_args(&s(&[])), Ok(Cmd::Help));
        assert_eq!(parse_args(&s(&["help"])), Ok(Cmd::Help));
        assert_eq!(parse_args(&s(&["run"])), Ok(Cmd::Run(Opts::default())));
        assert_eq!(
            parse_args(&s(&[
                "run", "--threads", "3", "--interval-ms", "250", "--trigger-pages", "64", "--min-swap-mb", "32",
                "--cooldown-secs", "10", "--max-psi", "25", "--mem-floor-pct", "15", "--verbose"
            ])),
            Ok(Cmd::Run(Opts {
                threads: 3,
                interval_ms: 250,
                trigger_pages: 64,
                min_swap_mb: 32,
                cooldown_secs: 10,
                max_psi: 25.0,
                floor_pct: 15,
                verbose: true
            }))
        );
        assert!(parse_args(&s(&["run", "--threads", "0"])).is_err());
        assert!(parse_args(&s(&["run", "--interval-ms", "10"])).is_err());
        assert!(parse_args(&s(&["run", "--trigger-pages"])).is_err());
        assert!(parse_args(&s(&["run", "--trigger-pages", "x"])).is_err());
        assert!(parse_args(&s(&["run", "chrome"])).is_err());
        assert!(parse_args(&s(&["run", "--pid", "5"])).is_err());
        assert_eq!(parse_args(&s(&["once", "Chrome"])), Ok(Cmd::Once(Opts::default(), Target::Name("chrome".into()))));
        assert_eq!(
            parse_args(&s(&["once", "--pid", "1234", "--threads", "2"])),
            Ok(Cmd::Once(Opts { threads: 2, ..Opts::default() }, Target::Pid(1234)))
        );
        assert!(parse_args(&s(&["once"])).is_err());
        assert!(parse_args(&s(&["once", "a", "b"])).is_err());
        assert!(parse_args(&s(&["once", "--pid", "0"])).is_err());
        assert!(parse_args(&s(&["bogus"])).is_err());
        assert_eq!(worker_count(&Opts::default(), 8), 7);
        assert_eq!(worker_count(&Opts::default(), 1), 1);
        assert_eq!(worker_count(&Opts { threads: 3, ..Opts::default() }, 8), 3);
    }

    #[test]
    fn budget_math() {
        let g = 1u64 << 30;
        // 16G total, 8G available: 8G − 1.6G floor − 128M margin.
        assert_eq!(budget(16 * g, 8 * g, 10, None), 8 * g - 16 * g / 100 * 10 - MARGIN);
        assert_eq!(budget(16 * g, g, 10, None), 0);
        assert_eq!(budget(16 * g, 8 * g, 10, Some(1000 * MB)), 800 * MB);
        assert_eq!(budget(16 * g, 8 * g, 10, Some(0)), 0);
    }

    // ── fake /sys/fs/cgroup + /proc tree ──

    struct Fake {
        root: PathBuf,
        sys: Sys,
    }

    impl Fake {
        fn new(name: &str) -> Fake {
            let root = std::env::temp_dir().join(format!("ultra-ram-pf-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("proc/pressure")).unwrap();
            fs::create_dir_all(root.join("sys/fs/cgroup/user.slice")).unwrap();
            let f = Fake { sys: Sys::under(&root), root };
            f.put("proc/meminfo", "MemTotal:       16777216 kB\nMemFree: 1 kB\nMemAvailable:    8388608 kB\n");
            f.put("proc/pressure/memory", "some avg10=0.00 avg60=0.00 avg300=0.00 total=0\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n");
            f
        }
        fn put(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
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

    #[test]
    fn discovery_and_limits() {
        let f = Fake::new("disc");
        let chrome = f.app(1000, "app-com.google.Chrome@abc.service");
        let tg = f.app(1000, "app-flatpak-org.telegram.desktop-1234.scope");
        let other = f.app(1001, "app-firefox-77.scope");
        fs::create_dir_all(f.sys.cgroup.join("user.slice/user-abc.slice")).unwrap();
        f.put("sys/fs/cgroup/user.slice/user-1002.slice", ""); // a file, not a dir
        f.put("sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/memory.stat", "");
        assert_eq!(uids(&f.sys), vec![1000, 1001]);
        assert_eq!(app_cgroups(&f.sys), vec![chrome.clone(), tg.clone(), other.clone()]);
        // --profile server: the server workloads instead, never the users' apps.
        let svc = f.sys.cgroup.join("system.slice/web-1.service");
        fs::create_dir_all(&svc).unwrap();
        let srv = Sys { server: Some(Discovery::default()), ..Sys::under(&f.root) };
        assert_eq!(app_cgroups(&srv), vec![svc.clone()]);
        assert_eq!(name_of(&chrome), "Chrome");
        assert_eq!(name_of(&tg), "telegram.desktop");

        f.put(&format!("{}/memory.stat", chrome.strip_prefix(&f.root).unwrap().display()),
            "anon 100\nswapcached 4096\nworkingset_refault_anon 777\npgmajfault 12\n");
        fs::write(chrome.join("memory.swap.current"), "1048576\n").unwrap();
        assert_eq!(counters(&chrome), Counters { refault: 777, majfault: 12, swap: MB });
        assert_eq!(usage(&chrome).swapcached, 4096);

        // Limits: none → None; memory.high on the app and memory.max on app.slice → the tighter.
        assert_eq!(headroom(&f.sys, &chrome), None);
        fs::write(chrome.join("memory.current"), format!("{}\n", 300 * MB)).unwrap();
        fs::write(chrome.join("memory.high"), format!("{}\n", 500 * MB)).unwrap();
        fs::write(chrome.join("memory.max"), "max\n").unwrap();
        assert_eq!(headroom(&f.sys, &chrome), Some(200 * MB));
        let slice = chrome.parent().unwrap();
        fs::write(slice.join("memory.current"), format!("{}\n", 900 * MB)).unwrap();
        fs::write(slice.join("memory.max"), format!("{}\n", 1000 * MB)).unwrap();
        assert_eq!(headroom(&f.sys, &chrome), Some(100 * MB));

        f.put("proc/42/cgroup", "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-com.google.Chrome@abc.service\n");
        assert_eq!(proc_cgroup(&f.sys, 42), Some(chrome.clone()));
        f.put("proc/43/cgroup", "0::/gone.scope\n");
        assert_eq!(proc_cgroup(&f.sys, 43), None);
        assert_eq!(mem(&f.sys), (16 * 1024 * MB, 8 * 1024 * MB));
        assert_eq!(global_psi10(&f.sys), 0.0);
    }

    #[test]
    fn wake_end_to_end() {
        let f = Fake::new("wake");
        let cg = f.app(1000, "app-com.google.Chrome@abc.service");
        fs::create_dir_all(cg.join("renderer")).unwrap();
        fs::write(cg.join("cgroup.procs"), "10\n").unwrap();
        fs::write(cg.join("renderer/cgroup.procs"), "11\n12\n").unwrap();
        fs::write(cg.join("memory.swap.current"), format!("{}\n", 200 * MB)).unwrap();
        fs::write(cg.join("memory.stat"), "swapcached 0\n").unwrap();
        let heap = |mb: u64| format!("10000000-{:x} rw-p 00000000 00:00 0 [heap]\nSwap: {} kB\nVmFlags: rd wr\n", 0x1000_0000 + mb * MB, mb * 1024);
        f.put("proc/10/smaps", &heap(20));
        f.put("proc/11/smaps", &heap(8));
        let rel = format!("0::/{}\n", cg.strip_prefix(&f.sys.cgroup).unwrap().display());
        f.put("proc/10/cgroup", &rel);
        f.put("proc/11/cgroup", &rel.replace("\n", "/renderer\n"));
        // 12 exits before we get to it.
        let mut pids = Vec::new();
        pids_in(&cg, &mut pids);
        pids.sort();
        assert_eq!(pids, vec![10, 11, 12]);

        let mut k = FakeK::ok();
        k.dead = vec![12];
        let w = Wake { name: "Chrome".into(), cg: Some(cg.clone()), pids };
        let r = wake(&f.sys, &k, &Opts::default(), &w, 3).unwrap();
        assert_eq!(r.skipped, None);
        assert_eq!(r.procs, 2);
        assert_eq!(r.planned, 28 * MB);
        assert_eq!(r.exec.chunks, 7);
        assert_eq!(r.exec.est, 28 * MB);
        assert_eq!(k.calls.lock().unwrap().len(), 7);
        let mut closed = k.closed.lock().unwrap().clone();
        closed.sort();
        assert_eq!(closed, vec![1010, 1011], "every opened pidfd closed");
        assert!(r.line(Some(300)).starts_with("⚡ wake Chrome: istənilən 28M"));

        // PID 11 was reused by a process of another cgroup after pids_in: its pidfd is closed
        // again and it gets no WILLNEED.
        f.put("proc/11/cgroup", "0::/user.slice/user-1001.slice/session-2.scope\n");
        let k = FakeK::ok();
        let r = wake(&f.sys, &k, &Opts::default(), &Wake { name: "Chrome".into(), cg: Some(cg.clone()), pids: vec![10, 11] }, 2).unwrap();
        assert_eq!((r.procs, r.planned), (1, 20 * MB));
        assert!(k.calls.lock().unwrap().iter().all(|c| c.0 == 1010));
        let mut closed = k.closed.lock().unwrap().clone();
        closed.sort();
        assert_eq!(closed, vec![1010, 1011]);
        assert!(!in_cgroup(&f.sys, 99, &cg), "no /proc entry");
        f.put("proc/11/cgroup", &rel.replace("\n", "/renderer\n"));

        // A memory.high limit on the app caps the wake to 80% of its headroom.
        fs::write(cg.join("memory.current"), format!("{}\n", 100 * MB)).unwrap();
        fs::write(cg.join("memory.high"), format!("{}\n", 125 * MB)).unwrap();
        let k = FakeK::ok();
        let r = wake(&f.sys, &k, &Opts::default(), &Wake { name: "Chrome".into(), cg: Some(cg.clone()), pids: vec![10, 11] }, 2).unwrap();
        assert_eq!(r.planned, 20 * MB);
        assert_eq!(r.cut, 8 * MB);
        assert!(r.line(None).contains("RAM büdcəsinə görə 8M buraxıldı"));
        fs::remove_file(cg.join("memory.high")).unwrap();

        // Low MemAvailable → skipped without a single syscall.
        f.put("proc/meminfo", "MemTotal: 16777216 kB\nMemAvailable: 1677721 kB\n");
        let k = FakeK::ok();
        let r = wake(&f.sys, &k, &Opts::default(), &Wake { name: "Chrome".into(), cg: Some(cg.clone()), pids: vec![10] }, 2).unwrap();
        assert!(r.skipped.as_deref().unwrap().starts_with("RAM azdır"));
        assert!(k.calls.lock().unwrap().is_empty());

        // Global PSI above the limit → skipped.
        f.put("proc/meminfo", "MemTotal: 16777216 kB\nMemAvailable: 8388608 kB\n");
        f.put("proc/pressure/memory", "some avg10=55.00 avg60=1.00 avg300=0.00 total=1\n");
        let r = wake(&f.sys, &k, &Opts::default(), &Wake { name: "Chrome".into(), cg: Some(cg.clone()), pids: vec![10] }, 2).unwrap();
        assert!(r.skipped.as_deref().unwrap().contains("PSI"));
        assert!(r.line(None).starts_with("· skip Chrome"));
        f.put("proc/pressure/memory", "some avg10=0.00 avg60=0.00 avg300=0.00 total=1\n");

        // Fatal kernel error surfaces as Err.
        let k = FakeK::with(|_, _| Err(EPERM));
        assert!(wake(&f.sys, &k, &Opts::default(), &Wake { name: "Chrome".into(), cg: None, pids: vec![10] }, 2).is_err());

        // Nothing in swap → skipped.
        f.put("proc/10/smaps", "10000000-10100000 rw-p 00000000 00:00 0 [heap]\nSwap: 0 kB\n");
        let r = wake(&f.sys, &FakeK::ok(), &Opts::default(), &Wake { name: "Chrome".into(), cg: None, pids: vec![10] }, 2).unwrap();
        assert!(r.skipped.is_some());
    }

    /// Real syscalls against a child `sleep`: needs root (CAP_SYS_NICE) and Linux ≥ 5.15.
    /// Run with `cargo test -- --ignored real_syscalls`.
    #[test]
    #[ignore]
    fn real_syscalls() {
        // Ok(Some(_)) in sandboxes whose PID 1 refuses ptrace-read; the child below is ours.
        let real = Real { verified: false };
        probe(&real).unwrap();
        let mut child = std::process::Command::new("sleep").arg("3").spawn().unwrap();
        thread::sleep(Duration::from_millis(100));
        let pid = child.id();
        let smaps = read(Path::new(&format!("/proc/{pid}/smaps")));
        // No swap needed: take every private anonymous region, swapped or not.
        let mut v: Vec<IoVec> = Vec::new();
        let mut lines = smaps.lines();
        while let Some(l) = lines.next() {
            if let Some((r, true)) = parse_header(l) {
                v.push(IoVec { base: r.start as usize, len: (r.end - r.start) as usize });
            }
        }
        assert!(!v.is_empty());
        let fds = FdSet::open(&real, &[pid]).unwrap();
        let fd = fds.fds[0].unwrap();
        let want: usize = v.iter().map(|x| x.len).sum();
        assert_eq!(real.willneed(fd, &v), Ok(want));
        assert_eq!(advise_iovs(&real, fd, &v), Outcome { bytes: want as u64, ..Default::default() });
        // An unmapped range: the kernel's error path (ENOMEM) is skipped, not fatal.
        let o = advise_iovs(&real, fd, &[IoVec { base: 4096, len: 4096 }]);
        assert_eq!((o.skipped, o.fatal, o.gone), (1, None, false), "{o:?}");
        let _ = child.wait();
        assert_eq!(real.willneed(fd, &v), Err(ESRCH), "exited process");
    }

    #[test]
    fn probe_maps_errors() {
        assert_eq!(probe(&FakeK::ok()), Ok(None));
        assert!(probe(&FakeK::with(|_, _| Err(EINVAL))).unwrap_err().contains("5.15"));
        assert!(probe(&FakeK::with(|_, _| Err(ENOSYS))).unwrap_err().contains("5.15"));
        assert!(probe(&FakeK::with(|_, _| Err(EPERM))).unwrap_err().contains("CAP_SYS_NICE"));
        assert!(probe(&FakeK::with(|_, _| Err(EACCES))).unwrap().is_some(), "unverifiable, not fatal");
        let k = FakeK::ok();
        probe(&k).unwrap();
        assert_eq!(*k.closed.lock().unwrap(), vec![1001]);
        assert_eq!(k.calls.lock().unwrap()[0].1, vec![IoVec { base: 0, len: 0 }]);
    }
}
