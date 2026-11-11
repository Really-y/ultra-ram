//! Server profile: workload discovery under the system and container cgroup roots, protection
//! of latency-sensitive services, and the operator status file (`/run/ultra-ram/status.json`).
//!
//! The reclaim engine (`pass()` in main.rs) is the same as on the desktop; only *what* counts as
//! an app differs. On a desktop an app is a child of the user's `app.slice`. On a server it is a
//! "leaf-ish" workload cgroup below one of the configured roots:
//! * a systemd unit: `system.slice/postgresql.service`, `system.slice/docker-<id>.scope`,
//!   `machine.slice/machine-qemu…scope`, `machine.slice/libpod-<id>.scope`;
//! * a Kubernetes pod: `kubepods.slice/kubepods-burstable.slice/kubepods-burstable-pod<uid>.slice`
//!   (systemd driver) or `kubepods/burstable/pod<uid>` (cgroupfs driver): the pod, not each of its
//!   containers, so the pause container and sidecars are reclaimed together with the app;
//! * a container of the cgroupfs driver: `docker/<64 hex>`;
//! * any other leaf directory below a root (e.g. `containerd/<namespace>/<id>`).
//!
//! `.slice` directories and plain directories with children are grouping levels and are
//! descended into, at most `depth` levels below a root. Never selected: `init.scope`, anything
//! named `user.slice` (the desktop profile owns it), units whose name starts with `ultra-ram`
//! (our own daemon and helpers) and the cgroup the daemon itself runs in. When two selected
//! cgroups nest (overlapping roots), only the outer one is kept: `memory.reclaim` on it covers
//! the inner one.
//!
//! The age and prefetch helpers call `workloads()` too when started with `--profile server`, so
//! all three see the same set.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use super::{h, pids_in, read};

pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
pub const RUN_DIR: &str = "/run/ultra-ram";
pub const CFG_DIR: &str = "/etc/ultra-ram";

/// Default discovery roots, relative to the cgroup2 mount. Roots that do not exist are skipped.
pub const DEFAULT_ROOTS: [&str; 7] =
    ["system.slice", "machine.slice", "kubepods.slice", "kubepods", "docker", "system.slice/docker-*.scope", "containerd"];

/// Latency-sensitive services that are never reclaimed from by default. Matched as a substring of
/// the workload's name and of the `comm` of every process in it (a postgres container is found by
/// its processes, its cgroup is only a hex id).
pub const DEFAULT_PROTECT: [&str; 11] = [
    "postgres",
    "mysqld",
    "mariadbd",
    "redis",
    "memcached",
    "etcd",
    "kubelet",
    "containerd",
    "dockerd",
    "sshd",
    "systemd-journald",
];

/// Process names of container runtimes' helpers, ignored when matching processes against the
/// protect list (`comm` is cut to 15 bytes: `containerd-shim-runc-v2` reads `containerd-shim`).
const RUNTIME_HELPERS: [&str; 9] =
    ["containerd-shim", "conmon", "runc", "crun", "pause", "docker-init", "tini", "dumb-init", "catatonit"];

/// Grouping levels descended below a root (kubepods.slice → QoS slice → pod needs 2).
pub const DEFAULT_DEPTH: usize = 4;
pub const MAX_DEPTH: usize = 8;
/// Safety caps: a broken or hostile tree must not make one scan unbounded.
const MAX_WORKLOADS: usize = 4096;
const MAX_ENTRIES_PER_DIR: usize = 16_384;
const MAX_PIDS_CHECKED: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct Discovery {
    pub roots: Vec<String>,
    pub depth: usize,
}

impl Default for Discovery {
    fn default() -> Self {
        Discovery { roots: DEFAULT_ROOTS.iter().map(|s| s.to_string()).collect(), depth: DEFAULT_DEPTH }
    }
}

// ───────────────────────────── discovery ─────────────────────────────

/// A root as given in the config → a clean relative path, or None when it is unusable: empty,
/// `.`/`..` components, a wildcard before the last component, or the desktop's `user.slice`.
pub fn clean_root(r: &str) -> Option<String> {
    let parts: Vec<&str> = r.trim().split('/').filter(|c| !c.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|c| *c == "." || *c == "..") {
        return None;
    }
    if parts[0] == "user.slice" || parts[0] == "init.scope" {
        return None;
    }
    if parts[..parts.len() - 1].iter().any(|c| c.contains('*')) {
        return None;
    }
    Some(parts.join("/"))
}

/// `*`-only wildcard match (no `?`, no classes).
pub fn wild(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if s.len() < first.len() + last.len() || !s.starts_with(first) || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir())
}

/// Child directories (symlinks are never followed), sorted, hidden names skipped.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .take(MAX_ENTRIES_PER_DIR)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn has_subdirs(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|rd| rd.flatten().any(|e| e.file_type().is_ok_and(|t| t.is_dir())))
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

fn is_unit(name: &str) -> bool {
    name.ends_with(".service") || name.ends_with(".scope")
}

fn is_container_id(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `kubepods-burstable-pod<uid>.slice`, `kubepods-pod<uid>.slice` (systemd driver: the uid's
/// dashes become underscores) or `pod<uid>` (cgroupfs driver).
pub fn is_pod(name: &str) -> bool {
    let id_ok = |s: &str| s.len() >= 8 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'_' || b == b'-');
    let base = name.strip_suffix(".slice").unwrap_or(name);
    if base.strip_prefix("pod").is_some_and(id_ok) {
        return true;
    }
    name.ends_with(".slice") && base.rsplit('-').next().and_then(|s| s.strip_prefix("pod")).is_some_and(id_ok)
}

fn is_workload_name(name: &str) -> bool {
    is_unit(name) || is_pod(name) || is_container_id(name)
}

fn excluded(name: &str) -> bool {
    name == "init.scope" || name == "user.slice" || name.starts_with("ultra-ram")
}

fn walk(dir: &Path, level: usize, d: &Discovery, self_cg: Option<&Path>, out: &mut Vec<PathBuf>) {
    for c in subdirs(dir) {
        if out.len() >= MAX_WORKLOADS {
            return;
        }
        let name = name_of(&c);
        if excluded(&name) {
            continue;
        }
        let mine = self_cg.is_some_and(|s| s.starts_with(&c));
        if is_workload_name(&name) {
            if !mine {
                out.push(c);
            }
        } else if has_subdirs(&c) {
            if level < d.depth {
                walk(&c, level + 1, d, self_cg, out);
            }
        } else if !name.ends_with(".slice") && !mine {
            // A plain leaf (cgroupfs container, a runtime's own layout). An empty slice is not a workload.
            out.push(c);
        }
    }
}

/// Every workload cgroup below the configured roots (absolute paths, sorted, no nesting).
pub fn workloads(cgroup: &Path, d: &Discovery, self_cg: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for r in d.roots.iter().filter_map(|r| clean_root(r)) {
        let last = r.rsplit('/').next().unwrap_or(&r).to_string();
        let full = cgroup.join(&r);
        let tops: Vec<PathBuf> = if last.contains('*') {
            full.parent().map(subdirs).unwrap_or_default().into_iter().filter(|p| wild(&last, &name_of(p))).collect()
        } else if is_real_dir(&full) {
            vec![full]
        } else {
            Vec::new()
        };
        for p in tops {
            let name = name_of(&p);
            if excluded(&name) {
                continue;
            }
            if is_workload_name(&name) {
                if !self_cg.is_some_and(|s| s.starts_with(&p)) {
                    out.push(p);
                }
            } else {
                walk(&p, 1, d, self_cg, &mut out);
            }
        }
    }
    out.sort();
    out.dedup();
    let set: BTreeSet<PathBuf> = out.iter().cloned().collect();
    out.retain(|p| !p.ancestors().skip(1).any(|a| set.contains(a)));
    out.truncate(MAX_WORKLOADS);
    out
}

/// The workload's path relative to the cgroup2 root: its stable key in status and history.
pub fn rel_key(cgroup: &Path, p: &Path) -> String {
    p.strip_prefix(cgroup).unwrap_or(p).to_string_lossy().to_string()
}

/// Short operator-facing name: unit suffix stripped, systemd's `\x2d` unescaped, 64-hex ids cut
/// to 12 characters (`docker-0123456789ab`).
pub fn workload_name(dir: &str) -> String {
    let mut s = dir.replace("\\x2d", "-");
    for suf in [".service", ".scope", ".slice"] {
        if let Some(x) = s.strip_suffix(suf) {
            s = x.to_string();
            break;
        }
    }
    s.split('-').map(|seg| if is_container_id(seg) { &seg[..12] } else { seg }).collect::<Vec<_>>().join("-")
}

// ───────────────────────────── protection ─────────────────────────────

/// Why a workload must not be reclaimed from, or None. Checked in this order: its name against
/// the protect list, `memory.min` / `memory.low` set by the operator (when `respect_low`), the
/// `comm` of its processes against the protect list. `comm` is injected for tests.
pub fn protection(
    dir: &Path,
    name: &str,
    protect: &[String],
    respect_low: bool,
    comm: &dyn Fn(u32) -> Option<String>,
) -> Option<String> {
    let n = name.to_lowercase();
    if let Some(p) = protect.iter().find(|p| !p.is_empty() && n.contains(p.as_str())) {
        return Some(format!("name:{p}"));
    }
    if respect_low {
        for f in ["memory.min", "memory.low"] {
            let v = read(&dir.join(f));
            let v = v.trim();
            if !v.is_empty() && v != "0" {
                return Some(f.to_string());
            }
        }
    }
    let mut pids = Vec::new();
    pids_in(dir, &mut pids);
    for pid in pids.into_iter().take(MAX_PIDS_CHECKED) {
        if let Some(c) = comm(pid) {
            let c = c.trim().to_lowercase();
            // Container plumbing can live inside a pod's cgroup; `containerd-shim` must not make
            // every pod look like containerd itself.
            if RUNTIME_HELPERS.iter().any(|r| c.starts_with(r)) {
                continue;
            }
            if let Some(p) = protect.iter().find(|p| !p.is_empty() && c.contains(p.as_str())) {
                return Some(format!("process:{p}"));
            }
        }
    }
    None
}

pub fn proc_comm(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm")).ok()
}

// ───────────────────────────── JSON (std-only) ─────────────────────────────

/// A JSON string literal, quotes included. Control characters, U+2028/U+2029 (break JavaScript
/// consumers) and DEL are written as `\uXXXX`.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Minimal streaming writer: tracks commas, so callers only say what comes next.
#[derive(Default)]
pub struct Json {
    s: String,
    first: Vec<bool>,
    after_key: bool,
}

impl Json {
    pub fn new() -> Json {
        Json::default()
    }
    fn sep(&mut self) {
        if self.after_key {
            self.after_key = false;
            return;
        }
        if let Some(f) = self.first.last_mut() {
            if *f {
                *f = false;
            } else {
                self.s.push(',');
            }
        }
    }
    pub fn obj(&mut self) -> &mut Self {
        self.sep();
        self.s.push('{');
        self.first.push(true);
        self
    }
    pub fn end_obj(&mut self) -> &mut Self {
        self.first.pop();
        self.s.push('}');
        self
    }
    pub fn arr(&mut self) -> &mut Self {
        self.sep();
        self.s.push('[');
        self.first.push(true);
        self
    }
    pub fn end_arr(&mut self) -> &mut Self {
        self.first.pop();
        self.s.push(']');
        self
    }
    pub fn key(&mut self, k: &str) -> &mut Self {
        self.sep();
        self.s.push_str(&escape(k));
        self.s.push(':');
        self.after_key = true;
        self
    }
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.sep();
        self.s.push_str(&escape(v));
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.sep();
        let _ = write!(self.s, "{v}");
        self
    }
    /// Non-finite numbers are not JSON: written as null.
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.sep();
        if v.is_finite() {
            let _ = write!(self.s, "{v:.3}");
        } else {
            self.s.push_str("null");
        }
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.sep();
        self.s.push_str(if v { "true" } else { "false" });
        self
    }
    pub fn null(&mut self) -> &mut Self {
        self.sep();
        self.s.push_str("null");
        self
    }
    pub fn opt_str(&mut self, v: Option<&str>) -> &mut Self {
        match v {
            Some(s) => self.str(s),
            None => self.null(),
        }
    }
    pub fn finish(self) -> String {
        self.s
    }
}

// ───────────────────────────── status ─────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct WorkloadRow {
    pub cgroup: String,
    pub name: String,
    pub memory: u64,
    pub swap: u64,
    pub reclaimed: u64,
    /// memory.pressure `some avg10` of the workload.
    pub psi10: f64,
    /// Stall share over the last tick, as `pass()` saw it (the back-off input).
    pub psi_tick: f64,
    /// Anonymous memory faulted back in since the daemon started watching it.
    pub refault: u64,
    /// Learned ceiling (% of the workload in swap), 0 = none yet.
    pub learned_cap: f64,
    pub state: &'static str,
    pub protected: Option<String>,
    pub leak: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub updated: u64,
    pub started: u64,
    pub gate_code: String,
    pub gate_msg: String,
    pub reclaimed_total: u64,
    pub mem_total: u64,
    pub mem_avail: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub zram_used: u64,
    pub psi_some10: f64,
    pub psi_some60: f64,
    pub psi_full10: f64,
    pub system_slice_psi10: f64,
    pub roots: Vec<String>,
    pub workloads: Vec<WorkloadRow>,
}

pub fn render_json(s: &Snapshot) -> String {
    let mut j = Json::new();
    j.obj();
    j.key("version").u64(1);
    j.key("profile").str("server");
    j.key("updated").u64(s.updated);
    j.key("started").u64(s.started);
    j.key("gate").obj().key("code").str(&s.gate_code).key("message").str(&s.gate_msg).end_obj();
    j.key("reclaimed_total_bytes").u64(s.reclaimed_total);
    j.key("memory").obj();
    j.key("total_bytes").u64(s.mem_total);
    j.key("available_bytes").u64(s.mem_avail);
    j.key("swap_total_bytes").u64(s.swap_total);
    j.key("swap_free_bytes").u64(s.swap_free);
    j.key("zram_ram_bytes").u64(s.zram_used);
    j.end_obj();
    j.key("psi").obj();
    j.key("system_some_avg10").f64(s.psi_some10);
    j.key("system_some_avg60").f64(s.psi_some60);
    j.key("system_full_avg10").f64(s.psi_full10);
    j.key("system_slice_some_avg10").f64(s.system_slice_psi10);
    j.end_obj();
    j.key("roots").arr();
    for r in &s.roots {
        j.str(r);
    }
    j.end_arr();
    j.key("workloads").arr();
    for w in &s.workloads {
        j.obj();
        j.key("cgroup").str(&w.cgroup);
        j.key("name").str(&w.name);
        j.key("state").str(w.state);
        j.key("protected").opt_str(w.protected.as_deref());
        j.key("memory_bytes").u64(w.memory);
        j.key("swap_bytes").u64(w.swap);
        j.key("reclaimed_bytes").u64(w.reclaimed);
        j.key("refault_bytes").u64(w.refault);
        j.key("psi_some_avg10").f64(w.psi10);
        j.key("psi_tick_pct").f64(w.psi_tick);
        if w.learned_cap > 0.0 {
            j.key("learned_cap_pct").f64(w.learned_cap);
        } else {
            j.key("learned_cap_pct").null();
        }
        j.key("leak_suspect").bool(w.leak);
        j.end_obj();
    }
    j.end_arr();
    j.end_obj();
    let mut out = j.finish();
    out.push('\n');
    out
}

/// The human-readable twin of the JSON, printed by `ultra-ram status --profile server`.
pub fn render_text(s: &Snapshot) -> String {
    let mut o = String::new();
    let up = s.updated.saturating_sub(s.started) / 60;
    let _ = writeln!(o, "Ultra RAM (server profile) — up {} h {} min, updated at {}", up / 60, up % 60, s.updated);
    let _ = writeln!(o, "State: {} ({})", s.gate_msg, s.gate_code);
    let _ = writeln!(o, "Offloaded since start: {}", h(s.reclaimed_total));
    let _ = writeln!(
        o,
        "RAM {} / {}, available {} | swap free {} / {} | zram RAM {} | PSI some10 {:.2} (system.slice {:.2})",
        h(s.mem_total.saturating_sub(s.mem_avail)),
        h(s.mem_total),
        h(s.mem_avail),
        h(s.swap_free),
        h(s.swap_total),
        h(s.zram_used),
        s.psi_some10,
        s.system_slice_psi10
    );
    let _ = writeln!(o, "Roots: {}", s.roots.join(", "));
    let _ = writeln!(
        o,
        "\n  {:<40} {:>7} {:>7} {:>9} {:>7} {:>8}  STATE",
        "WORKLOAD", "RAM", "SWAP", "RECLAIMED", "PSI10", "REFAULT"
    );
    for w in &s.workloads {
        let mut state = w.state.to_string();
        if let Some(p) = &w.protected {
            state = format!("{state} ({p})");
        }
        if w.leak {
            state.push_str(" LEAK SUSPECT");
        }
        let name = if w.cgroup.chars().count() > 40 { w.name.clone() } else { w.cgroup.clone() };
        let _ = writeln!(
            o,
            "  {:<40} {:>7} {:>7} {:>9} {:>7.2} {:>8}  {}",
            name,
            h(w.memory),
            h(w.swap),
            h(w.reclaimed),
            w.psi10,
            h(w.refault),
            state
        );
    }
    o
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Tree {
        root: PathBuf,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let root = std::env::temp_dir().join(format!("ultra-ram-server-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Tree { root }
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.root.join(rel);
            fs::create_dir_all(&p).unwrap();
            p
        }
        fn put(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
        fn rels(&self, v: &[PathBuf]) -> Vec<String> {
            v.iter().map(|p| rel_key(&self.root, p)).collect()
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    const POD: &str = "kubepods-burstable-pod0a1b2c3d_4e5f_6789_abcd_ef0123456789";
    const ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn sample(t: &Tree) {
        // systemd services, a template group slice, docker scopes, our own units, init.scope.
        for d in [
            "init.scope",
            "system.slice/postgres-bench.service",
            "system.slice/web-1.service",
            "system.slice/containerd.service",
            "system.slice/system-getty.slice/getty@tty1.service",
            "system.slice/system-empty.slice",
            &format!("system.slice/docker-{ID}.scope"),
            "system.slice/ultra-ram-server.service",
            "system.slice/ultra-ram-age.service",
            // the desktop's tree is never touched, even below a root that would match it
            "user.slice/user-1000.slice/user@1000.service/app.slice/app-firefox-1.scope",
            // VMs and podman
            "machine.slice/machine-qemu\\x2d1\\x2dvm1.scope/libvirt",
            "machine.slice/libpod-abc.scope/container",
            // Kubernetes, systemd driver: QoS slice → pod slice → container scopes
            &format!("kubepods.slice/kubepods-burstable.slice/{POD}.slice/cri-containerd-{ID}.scope"),
            &format!("kubepods.slice/kubepods-burstable.slice/{POD}.slice/cri-containerd-aaaa.scope"),
            "kubepods.slice/kubepods-pod11112222_3333_4444_5555_666677778888.slice/cri-containerd-bbbb.scope",
            "kubepods.slice/kubepods-besteffort.slice",
            // Kubernetes, cgroupfs driver
            "kubepods/burstable/pod12345678-aaaa-bbbb-cccc-1234567890ab/ctr1",
            // Docker, cgroupfs driver
            &format!("docker/{ID}"),
            // containerd, cgroupfs: namespace → container
            "containerd/default/redis-1",
        ] {
            t.dir(d);
        }
        t.put("system.slice/memory.stat", "anon 1\n"); // files are ignored
    }

    #[test]
    fn discovery_default_roots() {
        let t = Tree::new("disc");
        sample(&t);
        let self_cg = t.root.join("system.slice/ultra-ram-server.service");
        let got = t.rels(&workloads(&t.root, &Discovery::default(), Some(&self_cg)));
        assert_eq!(
            got,
            sv(&[
                "containerd/default/redis-1",
                &format!("docker/{ID}"),
                // paths sort by component: `kubepods` < `kubepods.slice`
                "kubepods/burstable/pod12345678-aaaa-bbbb-cccc-1234567890ab",
                &format!("kubepods.slice/kubepods-burstable.slice/{POD}.slice"),
                "kubepods.slice/kubepods-pod11112222_3333_4444_5555_666677778888.slice",
                "machine.slice/libpod-abc.scope",
                "machine.slice/machine-qemu\\x2d1\\x2dvm1.scope",
                "system.slice/containerd.service",
                &format!("system.slice/docker-{ID}.scope"),
                "system.slice/postgres-bench.service",
                "system.slice/system-getty.slice/getty@tty1.service",
                "system.slice/web-1.service",
            ])
        );
    }

    #[test]
    fn discovery_limits_and_roots() {
        let t = Tree::new("roots");
        sample(&t);
        // A glob root alone; user.slice, `..`, absolute junk and missing roots are refused or skipped.
        let d = Discovery {
            roots: sv(&["/system.slice/docker-*.scope/", "user.slice", "../etc", "*/x", "nope.slice", ""]),
            depth: 4,
        };
        assert_eq!(t.rels(&workloads(&t.root, &d, None)), vec![format!("system.slice/docker-{ID}.scope")]);
        // Depth 1: kubepods pods sit two grouping levels down and are not reached, the guaranteed pod is.
        let d = Discovery { roots: sv(&["kubepods.slice"]), depth: 1 };
        assert_eq!(
            t.rels(&workloads(&t.root, &d, None)),
            sv(&["kubepods.slice/kubepods-pod11112222_3333_4444_5555_666677778888.slice"])
        );
        // Overlapping roots: the outer cgroup wins, nothing appears twice.
        let d = Discovery { roots: sv(&["system.slice", "system.slice/web-1.service", "system.slice/docker-*.scope"]), depth: 4 };
        let got = t.rels(&workloads(&t.root, &d, None));
        assert_eq!(got.iter().filter(|r| r.contains("web-1")).count(), 1);
        assert_eq!(got.iter().filter(|r| r.contains("docker-")).count(), 1);
        // The daemon's own cgroup is skipped even without the ultra-ram name rule.
        t.dir("system.slice/mydaemon.service");
        let me = t.root.join("system.slice/mydaemon.service");
        let d = Discovery { roots: sv(&["system.slice"]), depth: 4 };
        assert!(!t.rels(&workloads(&t.root, &d, Some(&me))).iter().any(|r| r.contains("mydaemon")));
        assert!(t.rels(&workloads(&t.root, &d, None)).iter().any(|r| r.contains("mydaemon")));
        // A root pointing at the desktop tree through a symlink is not followed.
        std::os::unix::fs::symlink(t.root.join("user.slice"), t.root.join("sneaky")).unwrap();
        let d = Discovery { roots: sv(&["sneaky"]), depth: 8 };
        assert!(workloads(&t.root, &d, None).is_empty());
    }

    #[test]
    fn root_and_name_helpers() {
        assert_eq!(clean_root(" /system.slice// "), Some("system.slice".into()));
        assert_eq!(clean_root("user.slice/user-1000.slice"), None);
        assert_eq!(clean_root("a/../b"), None);
        assert_eq!(clean_root("a*/b"), None);
        assert_eq!(clean_root("/"), None);
        assert!(wild("docker-*.scope", "docker-abc.scope"));
        assert!(!wild("docker-*.scope", "docker-abc.service"));
        assert!(wild("*", "x") && wild("a*b*c", "aXbYc") && !wild("a*b*c", "aXc") && !wild("ab*ba", "aba"));
        assert!(is_pod(&format!("{POD}.slice")));
        assert!(is_pod("pod12345678-aaaa-bbbb-cccc-1234567890ab"));
        assert!(!is_pod("kubepods-burstable.slice") && !is_pod("podman.service") && !is_pod("pod"));
        assert_eq!(workload_name("postgres-bench.service"), "postgres-bench");
        assert_eq!(workload_name(&format!("docker-{ID}.scope")), "docker-0123456789ab");
        assert_eq!(workload_name("machine-qemu\\x2d1\\x2dvm1.scope"), "machine-qemu-1-vm1");
    }

    #[test]
    fn protection_rules() {
        let t = Tree::new("prot");
        let pg = t.dir("system.slice/postgres-bench.service");
        let web = t.dir("system.slice/web-1.service");
        let low = t.dir("system.slice/api.service");
        let min = t.dir("system.slice/batch.service");
        let ctr = t.dir(&format!("system.slice/docker-{ID}.scope"));
        let pod = t.dir(&format!("kubepods.slice/{POD}.slice/cri-containerd-x.scope"));
        let podd = pod.parent().unwrap().to_path_buf();
        for d in [&pg, &web, &low, &min, &ctr] {
            fs::write(d.join("memory.min"), "0\n").unwrap();
            fs::write(d.join("memory.low"), "0\n").unwrap();
        }
        fs::write(low.join("memory.low"), "268435456\n").unwrap();
        fs::write(min.join("memory.min"), "max\n").unwrap();
        // The container's cgroup is a hex id; its processes give it away.
        fs::write(ctr.join("cgroup.procs"), "101\n102\n").unwrap();
        fs::write(pod.join("cgroup.procs"), "201\n").unwrap();
        // A shim inside the web workload must not make it "containerd".
        fs::write(web.join("cgroup.procs"), "300\n301\n").unwrap();
        let comms: HashMap<u32, &str> =
            [(101, "tini\n"), (102, "redis-server\n"), (201, "postgres\n"), (300, "containerd-shim\n"), (301, "nginx\n")].into();
        let comm = |pid: u32| comms.get(&pid).map(|s| s.to_string());
        let list: Vec<String> = DEFAULT_PROTECT.iter().map(|s| s.to_string()).collect();
        let p = |d: &Path, respect: bool| protection(d, &workload_name(&name_of(d)), &list, respect, &comm);
        assert_eq!(p(&pg, true), Some("name:postgres".into()));
        assert_eq!(p(&web, true), None);
        assert_eq!(p(&low, true), Some("memory.low".into()));
        assert_eq!(p(&low, false), None, "operator intent can be switched off");
        assert_eq!(p(&min, true), Some("memory.min".into()));
        assert_eq!(p(&ctr, true), Some("process:redis".into()));
        assert_eq!(p(&podd, true), Some("process:postgres".into()), "pods are checked recursively");
        // Missing interface files (root cgroup, older kernels) mean "not set".
        let bare = t.dir("system.slice/bare.service");
        assert_eq!(p(&bare, true), None);
        // An empty protect entry never matches everything.
        assert_eq!(protection(&web, "web-1", &sv(&[""]), true, &comm), None);
    }

    #[test]
    fn json_escaping() {
        assert_eq!(escape("plain"), "\"plain\"");
        assert_eq!(escape("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(escape("l1\nl2\r\t\u{8}\u{c}"), "\"l1\\nl2\\r\\t\\b\\f\"");
        assert_eq!(escape("\u{0}\u{1f}\u{7f}"), "\"\\u0000\\u001f\\u007f\"");
        assert_eq!(escape("x\u{2028}y\u{2029}"), "\"x\\u2028y\\u2029\"");
        assert_eq!(escape("machine-qemu\\x2d1.scope ünïcode ✓"), "\"machine-qemu\\\\x2d1.scope ünïcode ✓\"");
    }

    #[test]
    fn json_writer() {
        let mut j = Json::new();
        j.obj().key("a").u64(1).key("b").arr().str("x").f64(1.5).f64(f64::NAN).bool(true).null().end_arr();
        j.key("c").obj().end_obj().key("d").arr().end_arr().key("e").opt_str(None).key("f").opt_str(Some("q\"")).end_obj();
        assert_eq!(j.finish(), r#"{"a":1,"b":["x",1.500,null,true,null],"c":{},"d":[],"e":null,"f":"q\""}"#);

        let s = Snapshot {
            updated: 100,
            started: 40,
            gate_code: "active".into(),
            gate_msg: "active — offloading".into(),
            roots: sv(&["system.slice"]),
            workloads: vec![
                WorkloadRow {
                    cgroup: "system.slice/a\"b.service".into(),
                    name: "a\"b".into(),
                    memory: 5,
                    state: "idle",
                    protected: Some("name:postgres".into()),
                    psi10: f64::INFINITY,
                    ..Default::default()
                },
                WorkloadRow { cgroup: "x".into(), name: "x".into(), state: "reclaiming", learned_cap: 33.0, ..Default::default() },
            ],
            ..Default::default()
        };
        let out = render_json(&s);
        assert!(out.ends_with("}\n"));
        assert!(out.starts_with(r#"{"version":1,"profile":"server","updated":100,"started":40,"gate":{"code":"active","#));
        assert!(out.contains(r#""cgroup":"system.slice/a\"b.service","name":"a\"b","state":"idle","protected":"name:postgres""#));
        assert!(out.contains(r#""psi_some_avg10":null"#), "non-finite numbers become null");
        assert!(out.contains(r#""learned_cap_pct":33.000"#) && out.contains(r#""learned_cap_pct":null"#));
        // Balanced brackets outside strings: a cheap structural check without a parser.
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for c in out.chars() {
            match (in_str, esc, c) {
                (true, true, _) => esc = false,
                (true, false, '\\') => esc = true,
                (true, false, '"') => in_str = false,
                (false, _, '"') => in_str = true,
                (false, _, '{' | '[') => depth += 1,
                (false, _, '}' | ']') => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0);
        }
        assert_eq!((depth, in_str), (0, false));
        let txt = render_text(&s);
        assert!(txt.contains("State: active — offloading (active)"));
        assert!(txt.contains("idle (name:postgres)"));
    }
}
