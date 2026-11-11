# Benchmark: Ultra RAM vs Wolfram vs kernel-only

A reproducible A/B/C benchmark of proactive memory offloaders on the same synthetic desktop workload.

| File | What it does |
|---|---|
| `fakeapp.c` | Synthetic "app": allocates anonymous memory filled with text-like data, keeps a **hot** set (read every 1 s), a **warm** set (read every 90 s) and a **cold** set (never read). It times every sweep, and on `SIGUSR1` reads the cold + warm sets once ("switching back to the app") and writes its stats. |
| `fakeserver.c` | Synthetic "service" for the server scenarios: same memory layout, but a client thread inside the process serves requests at a fixed rate. Each request touches K hot pages picked Zipf-like (a few pages constantly, the tail every few tens of seconds) and, 1 in 20, a few random warm pages. Latency is measured from each request's due time, so a stall also delays the requests queued behind it. `SIGUSR2` closes a measurement segment; `SIGUSR1` writes p50/p99/p99.9/max per segment, request count and major faults, then reads the cold set once ("cache warm-up after failover") and times it. |
| `run-scenario.sh` | Runs one scenario on a **fresh, throwaway** machine (it creates a user, a zram device and system services). |
| `report.py` | Turns the result directories into one comparison table (one for desktop, one for server scenarios). |

## Scenarios

| Scenario | What runs |
|---|---|
| `baseline` | Kernel only (zram swap, `swappiness=60`) |
| `wolfram` | Wolfram v0.3.0 with the config its `install.sh` generates for a desktop, run as its systemd unit |
| `ultra-ram` | Ultra RAM with default config, run as the same user unit `install.sh` writes |

Every scenario starts with zram sized to RAM using zstd, the way `zram-generator` sets it up on desktop distros.

## Timeline

1. Five background apps load: browser 900 MB, chat 600, launcher 450, editor 300, mail 250. They have different hot/warm/cold ratios.
2. **Run phase (900 s):** the daemon under test runs. In `baseline` there is no daemon.
3. **New app:** a 900 MB app is launched, which is more than the RAM still available. The script measures the load time and the memory stall.
4. **Switch back:** the daemon is frozen. The new app is closed, then each background app reads its cold + warm sets. This shows the price of whatever was offloaded.

## Running it

In a VM (recommended; the reference results were produced this way, with QEMU + the Arch Linux cloud image, 4 vCPU / 4 GB):

```bash
gcc -O2 -static -o fakeapp fakeapp.c
# put fakeapp, wolfram, ultra-ram, wolfram.service and run-scenario.sh in one dir, boot a fresh VM per scenario, then:
sudo RUN_SECS=900 ./run-scenario.sh ultra-ram /path/to/that/dir /path/to/results/ultra-ram
python3 report.py /path/to/results
```

`page_idle` must exist (`CONFIG_IDLE_PAGE_TRACKING=y`). Otherwise Wolfram runs in a degraded fallback mode. Arch, CachyOS and Fedora kernels have it; Debian kernels do not.

## Server scenarios

| Scenario | What runs |
|---|---|
| `server-baseline` | Kernel only (zram swap, `swappiness=60`) |
| `server-wolfram` | Wolfram with the config its `install.sh` generates for a generic server (headless, no Docker/Kubernetes/Proxmox), run as its systemd unit |
| `server-ultra-ram` | Ultra RAM's server profile as the system unit `install.sh --server` writes, with its shipped config except `max_offload_pct = 60`; disk-first tiering (1.4 GB disk swap, then the lz4 zram tier), the age helper and prefetch, both with `--profile server` |

Timeline:

1. Four services start as system units in `system.slice` (`systemd-run --unit=...`), 2500 MB in total, the same RAM pressure as the desktop apps:

   | Service | MB | hot / warm % | requests/s | hot pages per request |
   |---|---:|---:|---:|---:|
   | `postgres-bench` | 900 | 30 / 20 | 200 | 32 |
   | `web-1` | 600 | 10 / 20 | 200 | 16 |
   | `worker-1` | 500 | 5 / 15 | 50 | 16 |
   | `cache-1` | 500 | 20 / 30 | 400 | 8 |

   The database runs as a binary named `postgres-fake`. Wolfram's default exclude list matches process names containing `postgres`, and Ultra RAM protects the `postgres-bench` unit by name. So neither touches it, which is part of the comparison. Its latency shows what the others' offloading costs a protected neighbour.
2. **Run phase (900 s):** the services serve requests while the daemon under test runs.
3. **Deploy:** a 900 MB service (`deploy-1`) is started. The script measures its load time and the memory stall, and the services keep serving for another 120 s (their second latency segment).
4. **Failover:** the daemon is frozen. `deploy-1` is stopped, then each service stops serving and reads its cold set once. This shows the price of what was offloaded.

```bash
gcc -O2 -static -pthread -o fakeserver fakeserver.c -lm
# fakeserver, wolfram, wolfram.service, ultra-ram, run-scenario.sh in one dir; a fresh VM per scenario.
# server-ultra-ram also wants the 1.4 GB raw disk attached, as in the desktop disk run.
sudo RUN_SECS=900 ./run-scenario.sh server-ultra-ram /path/to/that/dir /path/to/results/server-ultra-ram
python3 report.py /path/to/results server-baseline server-wolfram server-ultra-ram
```

Knobs (environment): `SERVICES` (the table above, one `name MB hot% warm% req/s pages` line per service), `MAX_OFFLOAD` (60), `ACTIVATE` (unset: the server default `off`), `PREFETCH=` / `AGE=` (empty disables the helper), `AGE_INTERVAL` (150), `TRIGGER` (2000), `NO_COMPACT=` (empty keeps proactive compaction on).

## Results

Reference run: QEMU TCG (no KVM), Arch Linux kernel 7.2, 4 vCPU, 4 GB RAM. Swap: zram0 (zstd, size = RAM), zswap off. The Ultra RAM run uses all optional root parts:
- a 1.4 GB disk swap at the highest priority, attached as a separate raw image with the host page cache bypassed (`cache=none`) and throttled to 20k IOPS / 400 MB/s;
- the lz4 zram tier;
- prefetch;
- the MGLRU age helper.

Daemon config: `activate_avail_pct = 55`, `max_offload_pct = 60`.

Two runs each. Wolfram used its installer's desktop config. Senpai (Meta's open-source controller) ran with defaults, one instance per app.

| Metric | Wolfram | Senpai | No daemon | **Ultra RAM** |
|---|---:|---:|---:|---:|
| RAM gained during run | +495 / +491 MB | +613 MB | +3 MB | **+1262 / +1269 MB** |
| Memory refaulted during run | 238 / 238 MB | 48 MB | 0 | **0 / 0 MB** |
| App memory stall (app.slice PSI) | 0 / 0 ms | 1728 ms | 0 | 5 / 0 ms |
| Hot-set sweep, worst single sweep | 43 / 24 ms | 1868 ms | 38 ms | 62 / 25 ms |
| Warm-set sweep, worst | 1827 / 2081 ms | 213 ms | 14 ms | **11 / 15 ms** |
| Switch back to all 5 apps | 15.1 / 15.4 s | 33.5 s | 0.1 s | **5.0 / 5.4 s** |
| CPU of daemon + helpers during run | 56.5 / 54.6 s | 105.5 s | — | **23.3 / 23.6 s** |
| New 900 MB app load | 10.6 / 11.2 s | 11.1 s | 11.9 s | 10.9 / 12.0 s |
| Daemon peak memory | 17.8 / 18.5 MB | — | — | **2.0 / 2.2 MB** |

How to read it:
- **Most of the RAM lead comes from an architectural difference.** Neither Wolfram nor Senpai has a disk tier. Ultra RAM sends the coldest pages to disk, so they cost no RAM and no compression CPU.
- **Some rows are at the noise floor.** For the worst hot-set sweep and the new-app load time, the run-to-run spread of each tool is larger than the gap between tools, and the no-daemon run lands in the same range. Treat these rows as a tie.
- **TCG inflates CPU and time.** Absolute numbers are several times higher than on real hardware.

### Server results

Same VM. Four services in `system.slice` (the table above), two Ultra RAM runs, one each for Wolfram and the kernel alone. Wolfram used its installer's generic-server config. Ultra RAM used the server profile with all optional root parts (the 1.4 GB throttled disk swap first, then the lz4 zram tier; the age helper every 150 s; prefetch) and `max_offload_pct = 60`.

| Metric | Wolfram | No daemon | **Ultra RAM** |
|---|---:|---:|---:|
| RAM gained during run | +576 MB | +2 MB | **+942 / +924 MB** |
| Memory refaulted during run | 238 MB | 0 | **107 / 107 MB** |
| Worst request p99, run | 69.6 ms | 2.0 ms | **5.4 / 5.4 ms** |
| Worst request p99.9, run | 7728 ms | 32 ms | **30 / 22 ms** |
| Worst request max, run | 8646 ms | 246 ms | **300 / 238 ms** |
| Requests started >100 ms late | 2281 | 533 | **658 / 537** |
| Services' memory stall during run (PSI some) | 0 ms | 0 ms | 4401 / 4648 ms |
| Worst request p99.9 during deploy | 95 ms | 78 ms | 116 / 126 ms |
| 900 MB deploy load time | 14.9 s | 15.0 s | 14.3 / 14.0 s |
| Failover: cold-set read, all services | 24.1 s | 0.07 s | **3.4 / 3.5 s** |
| CPU of daemon + helpers during run | 91.0 s | — | **62.4 / 62.0 s** |
| Daemon peak memory | 14.1 MB | — | **2.7 / 2.7 MB** |

How to read it:
- Wolfram offloaded more (1.3 GB into zram) but zram kept 727 MB of it in RAM, and it took memory the services still read: a request waited up to 8.6 s, and the failover read took 24 s.
- Ultra RAM moved 1.0 GB to disk, which costs no RAM. Request latency stayed at the no-daemon level; the price is a small, steady memory stall (about 0.5% of the run) from reading rarely used data back from disk.
- Wolfram's PSI reads 0 although its requests stalled for seconds, so PSI does not capture its cost in this run. Compare the latency rows, not PSI, between the two.
- The database is protected by both tools, so its numbers show only what the neighbours' offloading costs it.

### Game scenario

`fakegame.c` is a synthetic game: it loads its assets, then renders at 60 fps on N threads (each frame a fixed amount of CPU work, calibrated to ~70% of a frame, plus reads across its whole memory), and loads one more level at a given time. It reports frame-time percentiles, late frames (> 33 ms), the level load time and its own major faults.

Put a `game.conf` next to the binaries (`<start_s> <base_mb> <level_mb> <level_at_s> <threads>`, the reference run used `60 1000 500 300 4`) and any desktop scenario starts the game as `app-gamescope-1` at that time and stops it at the end of the run. `report.py` then adds the game rows. Results are in the main README; one run each, so treat small differences as noise. The median frame time differs between runs because the per-frame work is calibrated at each start.

```bash
gcc -O2 -static -pthread -o fakegame fakegame.c -lm
```

## Caveats

- The reference VM ran under QEMU **TCG** (software emulation, no KVM), so absolute times are several times slower than real hardware. Compare scenarios with each other, not with your desktop.
- A synthetic workload has a perfectly cold set. Real apps are messier.
- Two runs per scenario for the reference table; differences inside the run-to-run spread are noise.
