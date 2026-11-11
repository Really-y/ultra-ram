# Installing and configuring Ultra RAM — instructions for an AI assistant

You are an AI coding assistant (Claude Code or similar) running on the user's own Linux machine. Your job: install Ultra RAM, tune its config to this machine, optionally add the root extras, and prove each step works. Follow the steps in order. Do not skip the checks.

## Ground rules

- **Ask before every `sudo` command** and say what it changes. The base install needs no root; the extras do.
- **One change at a time.** After each step, verify it before the next. If a check fails, stop and report; do not improvise workarounds on kernel or swap settings.
- **Never** turn off or resize the user's existing swap or zram, edit the bootloader, or disable Secure Boot. The tools here only add things.
- Keep a short log of what you ran and what it printed. At the end, report it to the user (template at the bottom).
- If anything looks wrong at any point: `ultra-ram rollback` pauses everything; `./install.sh --uninstall` removes everything.

## Step 0 — Inspect the machine (read-only)

Run these and keep the output:

```bash
cat /etc/os-release | head -3; uname -r
free -h
stat -fc %T /sys/fs/cgroup                    # must print cgroup2fs
cat /proc/pressure/memory                     # must exist (PSI)
swapon --show                                 # existing swap: NAME, TYPE, SIZE, PRIO
cat /sys/module/zswap/parameters/enabled 2>/dev/null
lsblk -d -o NAME,ROTA,SIZE,MODEL              # ROTA=0 means SSD/NVMe
cat /sys/kernel/mm/lru_gen/enabled 2>/dev/null   # 0x0007 = MGLRU with page-table walks
cat /sys/kernel/security/lockdown 2>/dev/null    # [none] is fine for the age helper
systemctl --user status >/dev/null && echo "user systemd OK"
```

Decide from it:

| Finding | Meaning |
|---|---|
| not `cgroup2fs`, or no `/proc/pressure/memory` | **Stop.** Ultra RAM cannot run here. Tell the user. |
| `swapon --show` is empty | There is nowhere to offload to. Ask the user whether to set up zram (on most distros: install `zram-generator`, defaults are fine) before going on. |
| Total RAM | Picks the config in Step 2. |
| A swap partition or swap file on a disk with `ROTA=0` | Disk-first tiering (Step 4c) is possible. On `ROTA=1` (spinning disk) **do not** use disk-first. |
| `lru_gen/enabled` is not `0x0007`, or lockdown is `integrity`/`confidentiality` | Skip the age helper (Step 4a). |

## Step 1 — Base install (rootless)

```bash
git clone https://github.com/Really-y/ultra-ram.git
cd ultra-ram
./install.sh
```

The installer checks compatibility and installs nothing if a check fails. It needs a Rust toolchain (`cargo`); if it is missing, ask the user to install it from their package manager (`rustup` or `rust`).

Verify:

```bash
systemctl --user is-active ultra-ram          # active
journalctl --user -u ultra-ram -n 20 --no-pager
ultra-ram status
ultra-ram analyze                             # where the RAM goes; save this as the "before" picture
```

The CLI output is in Azerbaijani. That is expected; translate the key lines for the user if they want.

## Step 2 — Config for this machine

Config file: `~/.config/ultra-ram/config`. It is reloaded within a minute; no restart needed. Change only the keys below, leave the rest at their defaults.

| RAM | `activate_avail_pct` | `max_offload_pct` | Why |
|---|---|---|---|
| ≤ 8 GB | `55` | `60` | Start earlier: RAM runs short often, and the benchmark (4 GB) used these values |
| 12–16 GB | `40` (default) | `50` (default) | Acts only when RAM gets short |
| ≥ 32 GB | `30` | `50` | Rarely needed; stay out of the way |

Edit with `sed` or an editor, e.g.:

```bash
sed -i 's/^activate_avail_pct = .*/activate_avail_pct = 55/; s/^max_offload_pct = .*/max_offload_pct = 60/' ~/.config/ultra-ram/config
```

### Games: make sure the game is protected

Game mode works only if the game's app cgroup matches the `protect` list (default `gamescope, wine, lutris, heroic`). Steam and Proton games often run inside **Steam's** cgroup. Check it:

1. Ask the user to start a game they play.
2. Run `ultra-ram watch` (or `ultra-ram analyze`) and find the row that holds the game's memory.
3. If its name does not contain one of the protect words, add a word from that name to `protect` (comma-separated).

Trade-off to tell the user: adding `steam` protects Steam and every game it launches, and whenever the Steam client is open (it uses a few hundred MB even when idle), Ultra RAM treats it as "a game is open" and keeps offloading background apps even while the CPUs are busy, at `SCHED_IDLE`.

## Step 3 — Watch it work (no root)

Leave it running through the user's normal work for at least 30 minutes, then:

```bash
ultra-ram status
ultra-ram analyze
journalctl --user -u ultra-ram --since "1 hour ago" --no-pager | tail -20
```

Healthy signs: apps show memory in zram, the log reports offloaded MB, PSI stays near 0, and the user has noticed no lag. If the user reports lag when switching back to an app, lower `max_offload_pct` by 10.

Many users can stop here. Continue only if the user wants more RAM back and agrees to root helpers.

## Step 4 — Root extras (optional, one at a time, ask first)

All three are separate system services. Install one, verify it, use the machine for a while, then the next. Each `--with-*` run also re-runs the base install; that is harmless.

### 4a. Age helper (most important)

Requires `lru_gen/enabled = 0x0007` and no kernel lockdown (Step 0).

```bash
./install.sh --with-age
systemctl is-active ultra-ram-age             # active
sudo ultra-ram age status                     # per app: generations and idle times
ls /run/ultra-ram-age/                        # one file per app after the first round (60 s)
```

The daemon picks the map up by itself. Keep `cold_age_secs` at its default (`120`) with the default 60 s interval.

### 4b. Prefetch (faster switch-back)

```bash
./install.sh --with-prefetch
systemctl is-active ultra-ram-prefetch
journalctl -u ultra-ram-prefetch -f           # switch to an app that sat idle: look for a "⚡ wake" line
```

### 4c. zram tier and disk-first layout

**Plain tier (any machine):**

```bash
./install.sh --with-zram-tier
systemctl is-active ultra-ram-zram-tier
ultra-ram zram-tier status
swapon --show                                 # a new zram device with the highest priority
```

The tier turns zswap off while it exists (zswap in front of zram would compress twice) and turns it back on when removed.

**Disk-first (only with an SSD swap partition or swap file, see Step 0):** this is the layout behind the largest gains in the benchmark. The coldest memory goes to the SSD swap and costs no RAM; the lz4 tier and the distro's zram take the rest. Swap with a higher priority fills first, so the order must be: disk swap > tier > distro zram.

1. Note the existing priorities from `swapon --show`. On many distros zram0 is `100`.
2. Give the disk swap the highest priority. **Ask the user first; this edits `/etc/fstab`.** Add or change `pri=300` in the options of that swap line, then:
   ```bash
   sudo swapoff <device-or-file> && sudo swapon -p 300 <device-or-file>
   ```
   Only do the `swapoff` if `free -h` shows enough free RAM to hold what is in that swap now.
3. Install the tier, then put it between the two with a drop-in:
   ```bash
   ./install.sh --with-zram-tier
   sudo systemctl edit ultra-ram-zram-tier
   ```
   with this content:
   ```ini
   [Service]
   ExecStartPre=
   ExecStartPre=/usr/local/bin/ultra-ram zram-tier setup --priority 200
   ```
   Then apply it: `sudo systemctl restart ultra-ram-zram-tier`. The restart removes the tier and sets it up again, and refuses to remove it if RAM is too short; if that happens, wait until more RAM is free and retry.
4. Verify the order: `swapon --show` lists disk swap `300`, the tier `200`, the distro zram lower.

## Step 5 — Final check and report

```bash
swapon --show
ultra-ram status
ultra-ram analyze
systemctl --no-pager status ultra-ram-age ultra-ram-prefetch ultra-ram-zram-tier 2>/dev/null | grep -E "●|Active:"
```

Compare `ultra-ram analyze` with the "before" picture from Step 1.

Report to the user:

```
Installed: base daemon [+ age] [+ prefetch] [+ zram tier, disk-first yes/no]
Config changed: <keys and values>, because <RAM size / games>
Games protected by: <protect entries>
Before → after: available RAM X → Y, in zram/swap Z, PSI <value>
Skipped: <step and why>
How to undo: ultra-ram rollback (pause) / ./install.sh --uninstall (remove)
```

## Server instead of desktop

For a server (services, containers, VMs, pods), use `./install.sh --server` instead of Step 1. Then:

1. Run `ultra-ram workloads` and show the user which workloads it would manage and which are protected **before** it has done much.
2. Make sure every database and latency-critical service is protected: either by name in `/etc/ultra-ram/config` (`protect = …`), or by giving it `memory.low` in its unit.
3. Add the extras with `./install.sh --server --with-root` only after the user agrees.
4. Watch it with `ultra-ram status --profile server` and `journalctl -u ultra-ram-server -f`.

The server defaults are already tuned (always on, PSI target 1.5%, cold = idle ≥ 300 s); do not apply the desktop table from Step 2.

## Undo

```bash
ultra-ram rollback              # remove limits, pause automation (stays installed)
./install.sh --uninstall        # remove the daemon and every root extra; keeps the config
rm -r ~/.config/ultra-ram       # remove the config too
```

If you changed swap priorities in `/etc/fstab`, tell the user; uninstall does not revert that.
