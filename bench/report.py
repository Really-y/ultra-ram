#!/usr/bin/env python3
"""Summarise run-scenario.sh results:  report.py <results-dir> [scenario ...]

Desktop scenarios (baseline, wolfram, ultra-ram, ...) and server scenarios (server-baseline,
server-wolfram, server-ultra-ram) get separate tables; a result directory is a server run when its
phase.txt has profile=server.
"""
import csv, os, re, sys

COLS = "t,avail_kb,zram_orig,zram_compr,zram_used,psi_some_us,psi_full_us,pswpin,pswpout,refault_anon,majflt,allocstall,zswap_kb,zswapped_kb,disk_swap_kb,compact_pages,user_psi_us".split(",")
APPS = ["browser", "chat", "launcher", "editor", "mail"]


def snap(line):
    v = line.strip().split(",")
    return {k: (float(x) if x not in ("", "x", "end", "end-run") else 0.0) for k, x in zip(COLS, v)}


def kv(path):
    out = {}
    if os.path.exists(path):
        for tok in re.findall(r"(\w+)=([\d.]+)", open(path).read()):
            out[tok[0]] = float(tok[1])
    return out


def load(d):
    rows = [snap(",".join(r)) for r in list(csv.reader(open(f"{d}/samples.csv")))[1:]]
    ph = {}
    for line in open(f"{d}/phase.txt"):
        k, _, v = line.strip().partition("=")
        if k.startswith("phase2") or k in ("game_start", "game_end"):
            ph[k] = snap(v)
        elif k in ("daemon_cpu_usec", "daemon_mem_peak", "helper_cpu_usec", "helper_cpu_end_usec", "game_t0", "game_t1"):
            ph[k] = float(v or 0)
    apps = {a: kv(f"{d}/{a}.txt") for a in APPS + ["newapp", "game"]}
    return rows, ph, apps


def summarise(d):
    rows, ph, apps = load(d)
    b, a = ph["phase2_before"], ph["phase2_after"]
    # first = when the daemon started; end_run = snapshot taken right before the new app launched
    first, end_run = rows[0], b
    s = {}
    s["avail_start_mb"] = first["avail_kb"] / 1024
    s["avail_end_mb"] = end_run["avail_kb"] / 1024
    s["ram_gained_mb"] = (end_run["avail_kb"] - first["avail_kb"]) / 1024
    stored = end_run["zram_orig"] + end_run.get("zswapped_kb", 0) * 1024
    used = end_run["zram_used"] + end_run.get("zswap_kb", 0) * 1024
    s["disk_mb"] = end_run.get("disk_swap_kb", 0) / 1024
    s["zram_orig_mb"] = stored / 2**20
    s["zram_used_mb"] = used / 2**20
    s["ratio"] = stored / used if stored > 2**20 and used else float("nan")
    s["run_psi_ms"] = (end_run["psi_some_us"] - first["psi_some_us"]) / 1000
    s["run_user_psi_ms"] = (end_run.get("user_psi_us", 0) - first.get("user_psi_us", 0)) / 1000
    s["compact_k"] = (a.get("compact_pages", 0) - first.get("compact_pages", 0)) / 1000
    s["run_refault_mb"] = (end_run["refault_anon"] - first["refault_anon"]) * 4 / 1024
    s["run_swapout_mb"] = (end_run["pswpout"] - first["pswpout"]) * 4 / 1024
    s["run_swapin_mb"] = (end_run["pswpin"] - first["pswpin"]) * 4 / 1024
    s["new_fill_ms"] = apps["newapp"].get("fill_ms", 0)
    s["new_psi_ms"] = (a["psi_some_us"] - b["psi_some_us"]) / 1000
    s["new_psi_full_ms"] = (a["psi_full_us"] - b["psi_full_us"]) / 1000
    s["new_allocstall"] = a["allocstall"] - b["allocstall"]
    s["hot_p99_ms"] = max(apps[x].get("hot_p99_ms", 0) for x in APPS)
    s["hot_max_ms"] = max(apps[x].get("hot_max_ms", 0) for x in APPS)
    s["warm_max_ms"] = max(apps[x].get("warm_max_ms", 0) for x in APPS)
    s["resume_ms"] = sum(apps[x].get("resume_ms", 0) for x in APPS)
    s["resume_majflt"] = sum(apps[x].get("majflt_resume", 0) for x in APPS)
    s["run_majflt"] = sum(apps[x].get("majflt_run", 0) for x in APPS)
    s["daemon_cpu_s"] = (ph.get("daemon_cpu_usec", 0) + ph.get("helper_cpu_usec", 0)) / 1e6
    s["resume_helper_cpu_s"] = (ph.get("helper_cpu_end_usec", 0) - ph.get("helper_cpu_usec", 0)) / 1e6
    s["daemon_mem_mb"] = ph.get("daemon_mem_peak", 0) / 2**20
    g = apps["game"]
    if g:
        gs, ge = ph.get("game_start"), ph.get("game_end")
        for k in ("p50_ms", "p99_ms", "p999_ms", "max_ms", "late", "level_ms", "avail_before_level_mb", "majflt", "frames"):
            s["game_" + k] = g.get(k, float("nan"))
        if gs and ge:
            s["game_psi_ms"] = (ge["psi_some_us"] - gs["psi_some_us"]) / 1000
            s["game_psi_full_ms"] = (ge["psi_full_us"] - gs["psi_full_us"]) / 1000
            s["game_allocstall"] = ge["allocstall"] - gs["allocstall"]
            s["game_avail_start_mb"] = gs["avail_kb"] / 1024
            t0, t1 = ph.get("game_t0", 0), ph.get("game_t1", 1e9)
            during = [r["avail_kb"] / 1024 for r in rows if t0 <= r["t"] <= t1]
            s["game_avail_min_mb"] = min(during) if during else float("nan")
    return s


ROWS = [
    ("Available RAM at start (MB)", "avail_start_mb", "{:.0f}"),
    ("Available RAM after run (MB)", "avail_end_mb", "{:.0f}"),
    ("**RAM gained during run (MB)**", "ram_gained_mb", "{:+.0f}"),
    ("Data in compressed RAM, zram+zswap (MB)", "zram_orig_mb", "{:.0f}"),
    ("Offloaded to disk swap (MB)", "disk_mb", "{:.0f}"),
    ("RAM used by compressed store (MB)", "zram_used_mb", "{:.0f}"),
    ("Compression ratio", "ratio", "{:.2f}x"),
    ("Swapped out during run (MB)", "run_swapout_mb", "{:.0f}"),
    ("Swapped back in during run (MB)", "run_swapin_mb", "{:.0f}"),
    ("Anon refaults during run (MB)", "run_refault_mb", "{:.0f}"),
    ("Memory stall during run, PSI some (ms)", "run_psi_ms", "{:.0f}"),
    ("Memory stall of apps only (app.slice), PSI some (ms)", "run_user_psi_ms", "{:.0f}"),
    ("Pages scanned/migrated by compaction, run+new app (k)", "compact_k", "{:.0f}"),
    ("Major faults in background apps during run", "run_majflt", "{:.0f}"),
    ("Hot-set sweep p99, worst app (ms)", "hot_p99_ms", "{:.1f}"),
    ("Hot-set sweep max, worst app (ms)", "hot_max_ms", "{:.1f}"),
    ("Warm-set sweep max, worst app (ms)", "warm_max_ms", "{:.0f}"),
    ("**New 900 MB app: load time (ms)**", "new_fill_ms", "{:.0f}"),
    ("New app: memory stall, PSI some (ms)", "new_psi_ms", "{:.0f}"),
    ("New app: memory stall, PSI full (ms)", "new_psi_full_ms", "{:.0f}"),
    ("New app: direct-reclaim stalls", "new_allocstall", "{:.0f}"),
    ("**Switch back to all 5 apps (ms)**", "resume_ms", "{:.0f}"),
    ("Switch back: major faults", "resume_majflt", "{:.0f}"),
    ("Daemon + root helpers CPU during run (s)", "daemon_cpu_s", "{:.1f}"),
    ("Helper CPU during switch-back (s)", "resume_helper_cpu_s", "{:.1f}"),
    ("Daemon peak memory (MB)", "daemon_mem_mb", "{:.1f}"),
]

GAME_ROWS = [
    ("Game: frames rendered", "game_frames", "{:.0f}"),
    ("Game: frame time p50 (ms)", "game_p50_ms", "{:.2f}"),
    ("**Game: frame time p99 (ms)**", "game_p99_ms", "{:.2f}"),
    ("**Game: frame time p99.9 (ms)**", "game_p999_ms", "{:.2f}"),
    ("Game: worst frame (ms)", "game_max_ms", "{:.0f}"),
    ("**Game: late frames (> 33 ms)**", "game_late", "{:.0f}"),
    ("Available RAM when the game starts (MB)", "game_avail_start_mb", "{:.0f}"),
    ("Available RAM, lowest while playing (MB)", "game_avail_min_mb", "{:.0f}"),
    ("Available RAM right before the level load (MB)", "game_avail_before_level_mb", "{:.0f}"),
    ("**Game: level load time (ms)**", "game_level_ms", "{:.0f}"),
    ("Game: major faults", "game_majflt", "{:.0f}"),
    ("Memory stall while playing, PSI some (ms)", "game_psi_ms", "{:.0f}"),
    ("Memory stall while playing, PSI full (ms)", "game_psi_full_ms", "{:.0f}"),
    ("Direct-reclaim stalls while playing", "game_allocstall", "{:.0f}"),
]

# ───────────────────────────── server scenarios ─────────────────────────────

SERVICES = ["postgres-bench", "web-1", "worker-1", "cache-1"]


def is_server(d):
    p = f"{d}/phase.txt"
    return os.path.exists(p) and any(l.strip() == "profile=server" for l in open(p))


def fakeserver_out(path):
    """fakeserver's output → {"head": {...}, "seg": {"0": {...}, "all": {...}}, "failover": {...}}."""
    out = {"head": {}, "seg": {}, "failover": {}}
    if not os.path.exists(path):
        return out
    for line in open(path):
        toks = dict(re.findall(r"(\w+)=([\w.]+)", line))
        if line.startswith("seg="):
            seg = toks.pop("seg")
            out["seg"][seg] = {k: float(v) for k, v in toks.items()}
        elif line.startswith("failover"):
            out["failover"] = {k: float(v) for k, v in toks.items()}
        elif line.strip():
            out["head"] = {k: float(v) for k, v in toks.items()}
    return out


def load_server(d):
    rows = [snap(",".join(r)) for r in list(csv.reader(open(f"{d}/samples.csv")))[1:]]
    ph, svc = {}, {}
    for line in open(f"{d}/phase.txt"):
        k, _, v = line.strip().partition("=")
        if k.startswith("phase2"):
            ph[k] = snap(v)
        elif k.startswith("svc."):
            _, phase, name = k.split(".", 2)
            vals = [float(x or 0) for x in v.split(",")]
            svc.setdefault(name, {})[phase] = dict(zip(("psi_us", "refault", "swap", "current"), vals + [0] * 4))
        elif k in ("daemon_cpu_usec", "daemon_mem_peak", "helper_cpu_usec", "helper_cpu_end_usec"):
            ph[k] = float(v or 0)
    names = [n for n in SERVICES if os.path.exists(f"{d}/{n}.txt")] or SERVICES
    out = {n: fakeserver_out(f"{d}/{n}.txt") for n in names + ["deploy-1"]}
    return rows, ph, svc, out, names


def summarise_server(d):
    rows, ph, svc, out, names = load_server(d)
    b, a = ph["phase2_before"], ph["phase2_after"]
    first, end_run = rows[0], b
    s = {"names": names}
    s["avail_start_mb"] = first["avail_kb"] / 1024
    s["avail_end_mb"] = end_run["avail_kb"] / 1024
    s["ram_gained_mb"] = (end_run["avail_kb"] - first["avail_kb"]) / 1024
    s["disk_mb"] = end_run.get("disk_swap_kb", 0) / 1024
    s["zram_orig_mb"] = end_run["zram_orig"] / 2**20
    s["zram_used_mb"] = end_run["zram_used"] / 2**20
    s["run_swapout_mb"] = (end_run["pswpout"] - first["pswpout"]) * 4 / 1024
    s["run_refault_mb"] = (end_run["refault_anon"] - first["refault_anon"]) * 4 / 1024
    s["run_psi_ms"] = (end_run["psi_some_us"] - first["psi_some_us"]) / 1000
    s["run_sys_slice_psi_ms"] = (end_run.get("user_psi_us", 0) - first.get("user_psi_us", 0)) / 1000

    def svc_delta(key, p0, p1):
        return sum(svc.get(n, {}).get(p1, {}).get(key, 0) - svc.get(n, {}).get(p0, {}).get(key, 0) for n in names)

    # The services' own stall excludes reclaim work done by a daemon that also lives in system.slice.
    s["run_svc_psi_ms"] = svc_delta("psi_us", "run_start", "run_end") / 1000
    s["run_svc_refault_mb"] = svc_delta("refault", "run_start", "run_end") * 4 / 1024
    s["svc_swap_mb"] = sum(svc.get(n, {}).get("run_end", {}).get("swap", 0) for n in names) / 2**20
    s["deploy_svc_psi_ms"] = svc_delta("psi_us", "run_end", "deploy_end") / 1000

    def lat(n, seg, key):
        return out[n]["seg"].get(seg, {}).get(key, float("nan"))

    for n in names:
        s[f"run_{n}"] = (lat(n, "0", "p99_ms"), lat(n, "0", "p999_ms"), lat(n, "0", "max_ms"))
        s[f"dep_{n}"] = (lat(n, "1", "p99_ms"), lat(n, "1", "p999_ms"), lat(n, "1", "max_ms"))
        s[f"fail_{n}"] = out[n]["failover"].get("cold_read_ms", float("nan"))
    worst = lambda seg, key: max((lat(n, seg, key) for n in names), default=float("nan"))
    s["run_worst_p99"], s["run_worst_p999"], s["run_worst_max"] = worst("0", "p99_ms"), worst("0", "p999_ms"), worst("0", "max_ms")
    s["dep_worst_p99"], s["dep_worst_p999"], s["dep_worst_max"] = worst("1", "p99_ms"), worst("1", "p999_ms"), worst("1", "max_ms")
    s["run_late"] = sum(out[n]["head"].get("late", 0) for n in names)
    s["svc_majflt"] = sum(out[n]["head"].get("majflt_serve", 0) for n in names)
    s["deploy_fill_ms"] = out["deploy-1"]["head"].get("fill_ms", float("nan"))
    s["deploy_psi_ms"] = (a["psi_some_us"] - b["psi_some_us"]) / 1000
    s["deploy_psi_full_ms"] = (a["psi_full_us"] - b["psi_full_us"]) / 1000
    s["deploy_allocstall"] = a["allocstall"] - b["allocstall"]
    s["failover_ms"] = sum(out[n]["failover"].get("cold_read_ms", 0) for n in names)
    s["failover_majflt"] = sum(out[n]["failover"].get("majflt_failover", 0) for n in names)
    s["daemon_cpu_s"] = (ph.get("daemon_cpu_usec", 0) + ph.get("helper_cpu_usec", 0)) / 1e6
    s["failover_helper_cpu_s"] = (ph.get("helper_cpu_end_usec", 0) - ph.get("helper_cpu_usec", 0)) / 1e6
    s["daemon_mem_mb"] = ph.get("daemon_mem_peak", 0) / 2**20
    return s


def triple(t):
    return " / ".join("—" if x != x else f"{x:.1f}" for x in t)


def server_rows(names):
    r = [
        ("Available RAM at start (MB)", "avail_start_mb", "{:.0f}"),
        ("Available RAM after run (MB)", "avail_end_mb", "{:.0f}"),
        ("**RAM gained during run (MB)**", "ram_gained_mb", "{:+.0f}"),
        ("Services' memory in swap after run (MB)", "svc_swap_mb", "{:.0f}"),
        ("Data in zram (MB)", "zram_orig_mb", "{:.0f}"),
        ("Offloaded to disk swap (MB)", "disk_mb", "{:.0f}"),
        ("RAM used by zram (MB)", "zram_used_mb", "{:.0f}"),
        ("Swapped out during run (MB)", "run_swapout_mb", "{:.0f}"),
        ("Anon refaults during run, system (MB)", "run_refault_mb", "{:.0f}"),
        ("Anon refaults during run, services (MB)", "run_svc_refault_mb", "{:.0f}"),
        ("Memory stall during run, PSI some, system (ms)", "run_psi_ms", "{:.0f}"),
        ("Memory stall during run, PSI some, system.slice (ms)", "run_sys_slice_psi_ms", "{:.0f}"),
        ("Memory stall during run, PSI some, the services only (ms)", "run_svc_psi_ms", "{:.0f}"),
        ("Major faults in the services while serving", "svc_majflt", "{:.0f}"),
        ("Requests started >100 ms late, all services", "run_late", "{:.0f}"),
    ]
    for n in names:
        r.append((f"{n}: request p99 / p99.9 / max, run (ms)", f"run_{n}", triple))
    r += [
        ("**Worst request p99, run (ms)**", "run_worst_p99", "{:.2f}"),
        ("**Worst request p99.9, run (ms)**", "run_worst_p999", "{:.2f}"),
        ("**Worst request max, run (ms)**", "run_worst_max", "{:.1f}"),
    ]
    for n in names:
        r.append((f"{n}: request p99 / p99.9 / max, deploy (ms)", f"dep_{n}", triple))
    r += [
        ("Worst request p99.9, deploy (ms)", "dep_worst_p999", "{:.2f}"),
        ("Worst request max, deploy (ms)", "dep_worst_max", "{:.1f}"),
        ("**Deploy: 900 MB service load time (ms)**", "deploy_fill_ms", "{:.0f}"),
        ("Deploy: memory stall, PSI some (ms)", "deploy_psi_ms", "{:.0f}"),
        ("Deploy: memory stall, PSI full (ms)", "deploy_psi_full_ms", "{:.0f}"),
        ("Deploy: direct-reclaim stalls", "deploy_allocstall", "{:.0f}"),
        ("Deploy: the services' own stall, PSI some (ms)", "deploy_svc_psi_ms", "{:.0f}"),
    ]
    for n in names:
        r.append((f"Failover: {n} cold-set read (ms)", f"fail_{n}", "{:.0f}"))
    r += [
        ("**Failover: cold-set read, all services (ms)**", "failover_ms", "{:.0f}"),
        ("Failover: major faults", "failover_majflt", "{:.0f}"),
        ("Daemon + root helpers CPU during run (s)", "daemon_cpu_s", "{:.1f}"),
        ("Helper CPU during failover (s)", "failover_helper_cpu_s", "{:.1f}"),
        ("Daemon peak memory (MB)", "daemon_mem_mb", "{:.1f}"),
    ]
    return r


def cell(fmt, v):
    if callable(fmt):
        return fmt(v)
    return "—" if isinstance(v, float) and v != v else fmt.format(v)


def table(scs, res, rows):
    print("| Metric | " + " | ".join(scs) + " |")
    print("|---|" + "---:|" * len(scs))
    for label, key, fmt in rows:
        print(f"| {label} | " + " | ".join(cell(fmt, res[sc].get(key, float("nan"))) for sc in scs) + " |")


if __name__ == "__main__":
    root = sys.argv[1]
    default = ("baseline", "wolfram", "ultra-ram", "server-baseline", "server-wolfram", "server-ultra-ram")
    scs = sys.argv[2:] or [s for s in default if os.path.exists(f"{root}/{s}/phase.txt")]
    desk = [sc for sc in scs if not is_server(f"{root}/{sc}")]
    srv = [sc for sc in scs if is_server(f"{root}/{sc}")]
    if desk:
        res = {sc: summarise(f"{root}/{sc}") for sc in desk}
        table(desk, res, ROWS + (GAME_ROWS if any("game_frames" in r for r in res.values()) else []))
    if srv:
        if desk:
            print()
        res = {sc: summarise_server(f"{root}/{sc}") for sc in srv}
        names = res[srv[0]]["names"]
        table(srv, res, server_rows(names))
