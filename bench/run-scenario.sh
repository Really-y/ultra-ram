#!/usr/bin/env bash
# Runs ONE benchmark scenario on a freshly booted machine (meant for a throwaway VM).
#
#   run-scenario.sh <baseline|wolfram|ultra-ram|ultra-ram-tier|ultra-ram-root|ultra-ram-disk|senpai
#                    |server-baseline|server-wolfram|server-ultra-ram> <dir-with-binaries> <result-dir>
#
# Needs root. Expects in <dir-with-binaries>: fakeapp (desktop) / fakeserver (server-*), wolfram,
# wolfram.service, ultra-ram.
# Desktop timeline (seconds after the background apps finished loading):
#   0 … RUN_SECS        daemon under test runs (baseline: kernel only)
#   RUN_SECS            a new 900 MB app is launched ("open a new app / tab")
#   RUN_SECS + 120      each background app is switched back to (its cold set is read)
# Server timeline (server-*; seconds after the services finished loading):
#   0 … RUN_SECS        four fakeserver services in system.slice serve requests, daemon under test runs
#   RUN_SECS            "deploy": an extra 900 MB service is started (load time, stall)
#   RUN_SECS + 120      "failover": the daemon is frozen, each service stops serving and reads its
#                       cold set once (a standby warming its cache)
set -uo pipefail
SC=$1; BIN=$2; OUT=$3
RUN_SECS=${RUN_SECS:-900}
U=bench
mkdir -p "$OUT"; exec > >(tee -a "$OUT/log.txt") 2>&1
echo "== scenario $SC  run=${RUN_SECS}s  $(uname -r)"

for b in fakeapp fakeserver fakegame wolfram ultra-ram; do [ -f "$BIN/$b" ] && install -m755 "$BIN/$b" /usr/local/bin/; done

# ── environment report ──
{
  echo "kernel=$(uname -r)"
  echo "cgroup=$(stat -fc %T /sys/fs/cgroup)"
  echo "page_idle=$([ -e /sys/kernel/mm/page_idle/bitmap ] && echo yes || echo no)"
  echo "psi=$([ -r /proc/pressure/memory ] && echo yes || echo no)"
  echo "mglru=$(cat /sys/kernel/mm/lru_gen/enabled 2>/dev/null || echo none)"
  grep -E 'MemTotal' /proc/meminfo
  nproc
} > "$OUT/env.txt"

# ── zram swap, the way desktop distros ship it (zram-generator: size = RAM, zstd) ──
modprobe zram num_devices=1 2>/dev/null || true
echo zstd > /sys/block/zram0/comp_algorithm 2>/dev/null || echo lz4 > /sys/block/zram0/comp_algorithm
echo "$(awk '/MemTotal/{print $2*1024}' /proc/meminfo)" > /sys/block/zram0/disksize
mkswap /dev/zram0 >/dev/null && swapon -p 100 /dev/zram0
# zswap in front of zram compresses everything twice and hides pages from zram; zram-generator
# setups disable it, and Wolfram disables it at start. Disable it for every scenario.
echo N > /sys/module/zswap/parameters/enabled 2>/dev/null || true
echo "zram0: $(cat /sys/block/zram0/comp_algorithm)  swappiness=$(cat /proc/sys/vm/swappiness)  zswap=$(cat /sys/module/zswap/parameters/enabled 2>/dev/null)"

# The cgroup whose memory stall goes into the last snap column: app.slice of the desktop user,
# or system.slice for the server scenarios. Set below, before the first snap.
APSI_CG=
snap() {  # one CSV line of system-wide counters
  local orig=0 compr=0 used=0
  for d in /sys/block/zram*; do read -r a b c _ < "$d/mm_stat"; orig=$((orig+a)); compr=$((compr+b)); used=$((used+c)); done
  local avail=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
  local psi=$(awk '/^some/{split($5,t,"=");print t[2]}' /proc/pressure/memory)
  local psif=$(awk '/^full/{split($5,t,"=");print t[2]}' /proc/pressure/memory)
  local dsk=$(awk '$1 ~ /^\/dev\/vd/ {u+=$4} END{print u+0}' /proc/swaps)
  local cm=$(awk '$1=="compact_migrate_scanned"||$1=="compact_daemon_migrate_scanned"||$1=="pgmigrate_success"{a+=$2} END{print a+0}' /proc/vmstat)
  local apsi=$(awk '/^some/{split($5,t,"=");print t[2]}' "$APSI_CG/memory.pressure" 2>/dev/null)
  local v=$(awk '$1=="pswpin"||$1=="pswpout"||$1=="workingset_refault_anon"||$1=="pgmajfault"||$1=="allocstall_normal"||$1=="allocstall_movable"{a[$1]=$2} END{print a["pswpin"]","a["pswpout"]","a["workingset_refault_anon"]","a["pgmajfault"]","a["allocstall_normal"]+a["allocstall_movable"]}' /proc/vmstat)
  local zs=$(awk '/^Zswap:/{a=$2} /^Zswapped:/{b=$2} END{print a+0","b+0}' /proc/meminfo)
  echo "$1,$avail,$orig,$compr,$used,$psi,$psif,$v,$zs,$dsk,$cm,${apsi:-0}"
}
SNAP_HEADER="t,avail_kb,zram_orig,zram_compr,zram_used,psi_some_us,psi_full_us,pswpin,pswpout,refault_anon,majflt,allocstall,zswap_kb,zswapped_kb,disk_swap_kb,compact_pages,user_psi_us"

# ═════════════════════════════ server scenarios ═════════════════════════════
if [[ $SC == server-* ]]; then
  RES=/tmp/res; mkdir -p $RES; chmod 777 $RES
  APSI_CG=/sys/fs/cgroup/system.slice
  SYS=/sys/fs/cgroup/system.slice
  echo "profile=server" >> "$OUT/phase.txt"
  # name  MB  hot%  warm%  req/s  hot-pages/request — 2500 MB in total, the same RAM pressure as
  # the desktop apps (MemAvailable ≈ 20–25% of a 4 GB VM once they are loaded).
  SERVICES=${SERVICES:-"postgres-bench 900 30 20 200 32
web-1 600 10 20 200 16
worker-1 500 5 15 50 16
cache-1 500 20 30 400 8"}
  NAMES=$(awk '{print $1}' <<< "$SERVICES")
  # The "database" runs as a binary named postgres-fake: Wolfram's default exclude list matches
  # process names containing "postgres", Ultra RAM's server profile protects the unit by name.
  # Both leave it alone; that is part of the comparison.
  install -m755 /usr/local/bin/fakeserver /usr/local/bin/postgres-fake
  svc_bin() { case $1 in postgres*) echo /usr/local/bin/postgres-fake ;; *) echo /usr/local/bin/fakeserver ;; esac; }
  start_svc() {  # name MB hot% warm% req/s pages
    systemd-run --quiet --unit="$1" --slice=system.slice "$(svc_bin "$1")" "$2" "$3" "$4" "$5" "$6" "$RES/$1.txt"
  }
  svc_stats() {  # per service: memory PSI some total (us), anon refaults (pages), swap, current
    for n in $NAMES ${1:+$1}; do
      local d=$SYS/$n.service
      [ -d "$d" ] || continue
      echo "svc.$2.$n=$(awk '/^some/{split($5,t,"=");print t[2]}' "$d/memory.pressure"),$(awk '$1=="workingset_refault_anon"{print $2}' "$d/memory.stat"),$(cat "$d/memory.swap.current"),$(cat "$d/memory.current")"
    done
  }
  while read -r n mb hp wp rq k; do start_svc "$n" "$mb" "$hp" "$wp" "$rq" "$k"; done <<< "$SERVICES"
  for n in $NAMES; do
    for _ in $(seq 1800); do [ -f "$RES/$n.txt.ready" ] && break; sleep 1; done
  done
  echo "services loaded: $(cat $RES/*.ready | tr '\n' ' ')"
  sleep 20
  echo "$SNAP_HEADER" > "$OUT/samples.csv"
  HELPERS=""
  case $SC in
    server-wolfram)
      mkdir -p /etc/wolfram /var/lib/wolfram
      # Exactly what Wolfram's install.sh writes for a headless machine without Docker, Kubernetes
      # or Proxmox ("Detected generic server — using standard config"). postgres is excluded by name.
      cat > /etc/wolfram/config.toml <<'EOF'
min_process_anon_mib = 50
min_region_kib = 4096
idle_sample_secs = 30
cycle_secs = 120
cold_threshold = 0.7
pageout_threshold = 0.9
pageout_after_cycles = 3
enable_hibernation = false
hibernate_after_cycles = 10
checkpoint_dir = "/var/lib/wolfram/checkpoints"
enable_port_proxy = false
excluded_names = ["systemd", "wolfram", "sshd", "dockerd", "containerd", "containerd-shim", "kubelet", "kube-proxy", "kube-apiserver", "kube-controller", "kube-scheduler", "etcd", "postgres", "mysqld", "mariadbd", "llama"]
excluded_pids = [1]
dry_run = false
skip_gpu_processes = true
refault_backoff_threshold = 0.25
refault_backoff_cycles = 10
EOF
      cp "$BIN/wolfram.service" /etc/systemd/system/wolfram.service
      systemctl daemon-reload; systemctl start wolfram
      DCG=$SYS/wolfram.service ;;
    server-ultra-ram)
      # Disk-first tiering as in the desktop ultra-ram-disk run: coldest pages to a size-capped disk
      # swap (highest priority), overflow to the lz4 zram tier, then the zstd zram0.
      DISK=$(lsblk -dpno NAME,SIZE | awk '$2=="1.4G"{print $1}' | head -1)
      # ${X-1}, not ${X:-1}: an explicitly empty X= switches the step off.
      [ -n "${NO_COMPACT-1}" ] && echo 0 > /proc/sys/vm/compaction_proactiveness
      if [ -n "$DISK" ]; then echo "swap disk: $DISK"; mkswap "$DISK" >/dev/null && swapon -p 300 "$DISK"; else echo "! no 1.4G disk found: running without the disk tier"; fi
      /usr/local/bin/ultra-ram zram-tier setup --priority 200 --no-recomp; /usr/local/bin/ultra-ram zram-tier status
      mkdir -p /etc/ultra-ram
      # The server profile's shipped config; only the offload cap differs, as in the desktop disk run.
      /usr/local/bin/ultra-ram print-default-config --profile server \
        | sed -e "s/^max_offload_pct = .*/max_offload_pct = ${MAX_OFFLOAD:-60}/" \
              ${ACTIVATE:+-e "s/^activate_avail_pct = .*/activate_avail_pct = $ACTIVATE/"} > /etc/ultra-ram/config
      if [ -n "${PREFETCH-1}" ]; then
        systemd-run --quiet --unit=uram-prefetch /usr/local/bin/ultra-ram prefetch run --profile server --verbose --trigger-pages ${TRIGGER:-2000}
        HELPERS="$HELPERS $SYS/uram-prefetch.service"
      fi
      if [ -n "${AGE-1}" ]; then
        systemd-run --quiet --unit=uram-age -p Nice=10 /usr/local/bin/ultra-ram age run --profile server --interval-secs ${AGE_INTERVAL:-150} --verbose
        HELPERS="$HELPERS $SYS/uram-age.service"
      fi
      cat /proc/swaps
      # Same unit install.sh --server writes.
      cat > /etc/systemd/system/ultra-ram-server.service <<'EOF'
[Unit]
Description=Ultra RAM server profile — proactive cold-memory offload for services, containers and pods
After=swap.target
StartLimitIntervalSec=900
StartLimitBurst=3

[Service]
Type=simple
ExecStart=/usr/local/bin/ultra-ram daemon --profile server
Restart=on-failure
RestartSec=30
Nice=10
CPUWeight=20
IOSchedulingClass=best-effort
IOSchedulingPriority=7
MemoryMax=64M
CapabilityBoundingSet=
AmbientCapabilities=
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectClock=yes
ProtectHostname=yes
RestrictAddressFamilies=AF_UNIX
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
UMask=0022
RuntimeDirectory=ultra-ram
RuntimeDirectoryMode=0755

[Install]
WantedBy=multi-user.target
EOF
      systemctl daemon-reload; systemctl start ultra-ram-server
      DCG=$SYS/ultra-ram-server.service ;;
    *) DCG= ;;
  esac

  svc_stats "" run_start >> "$OUT/phase.txt"
  T0=$(date +%s)
  el() { echo $(( $(date +%s) - T0 )); }
  snap 0 >> "$OUT/samples.csv"
  while [ "$(el)" -lt "$RUN_SECS" ]; do sleep 10; snap "$(el)" >> "$OUT/samples.csv"; done
  svc_stats "" run_end >> "$OUT/phase.txt"
  # Close the "run" latency segment of every service; what follows is the deploy segment.
  for n in $NAMES; do systemctl kill -s SIGUSR2 "$n.service"; done

  # ── deploy: an extra 900 MB service while the others keep serving ──
  echo "deploying deploy-1 at $(el)s"
  B=$(snap x)
  start_svc deploy-1 900 20 20 100 16
  for _ in $(seq 1800); do [ -f "$RES/deploy-1.txt.ready" ] && break; sleep 1; snap "$(el)" >> "$OUT/samples.csv"; done
  echo "deploy-1 $(cat $RES/deploy-1.txt.ready)"
  A=$(snap x)
  echo "phase2_before=$B" >> "$OUT/phase.txt"; echo "phase2_after=$A" >> "$OUT/phase.txt"
  T2=$(date +%s); while [ $(( $(date +%s) - T2 )) -lt 120 ]; do sleep 10; snap "$(el)" >> "$OUT/samples.csv"; done
  svc_stats deploy-1 deploy_end >> "$OUT/phase.txt"

  # ── daemon cost ──
  if [ -n "$DCG" ] && [ -r "$DCG/cpu.stat" ]; then
    echo "daemon_cpu_usec=$(awk '/usage_usec/{print $2}' "$DCG/cpu.stat")" >> "$OUT/phase.txt"
    echo "daemon_mem_peak=$(cat "$DCG/memory.peak" 2>/dev/null)" >> "$OUT/phase.txt"
  fi
  hc=0; for h in $HELPERS; do hc=$((hc + $(awk '/usage_usec/{print $2}' "$h/cpu.stat" 2>/dev/null || echo 0))); done
  echo "helper_cpu_usec=$hc" >> "$OUT/phase.txt"
  case $SC in
    server-wolfram) cp /var/lib/wolfram/stats.json "$OUT/" 2>/dev/null; journalctl -u wolfram --no-pager > "$OUT/daemon.log" ;;
    server-ultra-ram)
      /usr/local/bin/ultra-ram zram-tier status > "$OUT/tier-status.txt" 2>&1
      /usr/local/bin/ultra-ram status --profile server > "$OUT/daemon-status.txt" 2>&1
      cp /run/ultra-ram/status.json "$OUT/" 2>/dev/null
      journalctl -u ultra-ram-server --no-pager > "$OUT/daemon.log" ;;
  esac
  # Freeze the daemon so the failover measures the state it left behind (helpers keep running).
  case $SC in server-wolfram) systemctl kill -s SIGSTOP wolfram ;; server-ultra-ram) systemctl kill -s SIGSTOP ultra-ram-server ;; esac
  snap end-run >> "$OUT/phase.txt"

  # ── failover: stop the deploy, then each service stops serving and reads its cold set once ──
  for n in deploy-1 $NAMES; do
    systemctl kill -s SIGUSR1 "$n.service"
    for _ in $(seq 1800); do grep -q '^failover' "$RES/$n.txt" 2>/dev/null && break; sleep 1; done
  done
  cp $RES/*.txt "$OUT/"
  hc=0; for h in $HELPERS; do hc=$((hc + $(awk '/usage_usec/{print $2}' "$h/cpu.stat" 2>/dev/null || echo 0))); done
  echo "helper_cpu_end_usec=$hc" >> "$OUT/phase.txt"
  journalctl -u uram-prefetch -u uram-age --no-pager > "$OUT/helpers.log" 2>/dev/null
  /usr/local/bin/ultra-ram zram-tier status > "$OUT/tier-status-end.txt" 2>&1
  snap end >> "$OUT/phase.txt"
  echo "== done $SC"
  exit 0
fi

# ═════════════════════════════ desktop scenarios ═════════════════════════════
# ── desktop user with a systemd user manager (apps land in app.slice) ──
id $U >/dev/null 2>&1 || useradd -m -s /bin/bash $U
UID_B=$(id -u $U)
loginctl enable-linger $U
for _ in $(seq 120); do [ -S /run/user/$UID_B/bus ] && break; sleep 1; done
asu() { runuser -u $U -- env XDG_RUNTIME_DIR=/run/user/$UID_B DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$UID_B/bus "$@"; }
RES=/tmp/res; mkdir -p $RES; chmod 777 $RES

# name  MB  hot%  warm%
APPS="browser 900 15 25
chat 600 10 20
launcher 450 5 15
editor 300 30 30
mail 250 10 20"

start_app() { asu systemd-run --user --quiet --unit="app-$1-1" /usr/local/bin/fakeapp "$2" "$3" "$4" "$RES/$1.txt"; }
while read -r n mb hp wp; do start_app "$n" "$mb" "$hp" "$wp"; done <<< "$APPS"
for n in $(awk '{print $1}' <<< "$APPS"); do
  for _ in $(seq 1800); do [ -f "$RES/$n.txt.ready" ] && break; sleep 1; done
done
echo "apps loaded: $(cat $RES/*.ready | tr '\n' ' ')"
sleep 20

APSI_CG=/sys/fs/cgroup/user.slice/user-$UID_B.slice/user@$UID_B.service/app.slice
echo "$SNAP_HEADER" > "$OUT/samples.csv"

# ── start the daemon under test ──
case $SC in
  wolfram)
    mkdir -p /etc/wolfram /var/lib/wolfram
    # Exactly what Wolfram's install.sh writes for a desktop.
    cat > /etc/wolfram/config.toml <<'EOF'
min_process_anon_mib = 50
min_region_kib = 2048
idle_sample_secs = 30
cycle_secs = 120
cold_threshold = 0.7
pageout_threshold = 0.9
pageout_after_cycles = 3
enable_hibernation = false
hibernate_after_cycles = 10
checkpoint_dir = "/var/lib/wolfram/checkpoints"
enable_port_proxy = false
excluded_names = ["systemd", "wolfram", "sshd", "dockerd", "containerd", "containerd-shim", "kubelet", "kube-proxy", "kube-apiserver", "kube-controller", "kube-scheduler", "etcd", "postgres", "mysqld", "mariadbd", "llama", "Xorg", "Xwayland", "gnome-shell", "kwin_wayland", "kwin_x11", "plasmashell", "sway", "hyprland", "mutter", "pipewire", "pulseaudio", "wireplumber", "mutter"]
excluded_pids = [1]
dry_run = false
skip_gpu_processes = true
refault_backoff_threshold = 0.25
refault_backoff_cycles = 10
EOF
    cp "$BIN/wolfram.service" /etc/systemd/system/wolfram.service
    systemctl daemon-reload; systemctl start wolfram
    DCG=/sys/fs/cgroup/system.slice/wolfram.service ;;
  ultra-ram|ultra-ram-tier|ultra-ram-root|ultra-ram-disk)
    if [ "$SC" = ultra-ram-disk ]; then
      # Disk-first tiering: coldest pages go to a size-capped disk swap (highest priority, no
      # compression CPU), overflow to the lz4 zram tier, then the distro's zstd zram0.
      DISK=$(lsblk -dpno NAME,SIZE | awk '$2=="1.4G"{print $1}' | head -1)
      # Same knob Wolfram sets: freeing GBs of scattered 4K pages makes proactive compaction
      # migrate pages (including hot ones) in the background. Testing whether that is what slowed v6.
      [ -n "${NO_COMPACT:-1}" ] && echo 0 > /proc/sys/vm/compaction_proactiveness
      echo "swap disk: $DISK"; mkswap "$DISK" >/dev/null && swapon -p 300 "$DISK"
      /usr/local/bin/ultra-ram zram-tier setup --priority 200 --no-recomp; /usr/local/bin/ultra-ram zram-tier status
      systemd-run --quiet --unit=uram-prefetch /usr/local/bin/ultra-ram prefetch run --verbose --trigger-pages ${TRIGGER:-2000}
      HELPERS="/sys/fs/cgroup/system.slice/uram-prefetch.service"
      if [ -n "${AGE:-1}" ]; then
        # MGLRU aging helper: reclaim then takes only pages idle >= cold_age_secs.
        systemd-run --quiet --unit=uram-age -p Nice=10 /usr/local/bin/ultra-ram age run --interval-secs ${AGE_INTERVAL:-60} --verbose
        HELPERS="$HELPERS /sys/fs/cgroup/system.slice/uram-age.service"
      fi
      mkdir -p /home/$U/.config/ultra-ram
      /usr/local/bin/ultra-ram print-default-config | sed -e "s/^max_offload_pct = .*/max_offload_pct = ${MAX_OFFLOAD:-60}/" -e "s/^activate_avail_pct = .*/activate_avail_pct = 55/" > /home/$U/.config/ultra-ram/config
      cat /proc/swaps
    elif [ "$SC" != ultra-ram ]; then
      # Optional root part: lz4 zram tier above the distro's zstd zram0.
      /usr/local/bin/ultra-ram zram-tier setup; /usr/local/bin/ultra-ram zram-tier status
    fi
    if [ "$SC" = ultra-ram-root ]; then
      # All root helpers: zstd recompression of idle pages + swap-in ahead of a waking app.
      systemd-run --quiet --unit=uram-maint -p Nice=19 /usr/local/bin/ultra-ram zram-tier maintain --loop 60 --idle-secs ${IDLE_SECS:-300}
      systemd-run --quiet --unit=uram-prefetch /usr/local/bin/ultra-ram prefetch run --verbose
      HELPERS="/sys/fs/cgroup/system.slice/uram-maint.service /sys/fs/cgroup/system.slice/uram-prefetch.service"
      mkdir -p /home/$U/.config/ultra-ram
      /usr/local/bin/ultra-ram print-default-config | sed "s/^max_offload_pct = .*/max_offload_pct = ${MAX_OFFLOAD:-70}/" > /home/$U/.config/ultra-ram/config
    fi
    mkdir -p /home/$U/.config/systemd/user
    # Same unit Ultra RAM's install.sh writes.
    cat > /home/$U/.config/systemd/user/ultra-ram.service <<'EOF'
[Service]
ExecStart=/usr/local/bin/ultra-ram daemon
Restart=on-failure
Slice=background.slice
Nice=19
IOSchedulingClass=idle
MemoryMax=64M
NoNewPrivileges=yes
EOF
    chown -R $U: /home/$U/.config
    asu systemctl --user daemon-reload; asu systemctl --user start ultra-ram.service
    DCG=/sys/fs/cgroup/user.slice/user-$UID_B.slice/user@$UID_B.service/background.slice/ultra-ram.service ;;
  senpai)
    # Meta's Senpai (facebookincubator/senpai, PSI-driven memory.high controller), default settings,
    # one instance per app cgroup — the way it is deployed per workload.
    APPCG=/sys/fs/cgroup/user.slice/user-$UID_B.slice/user@$UID_B.service/app.slice
    HELPERS=""
    for n in $(awk '{print $1}' <<< "$APPS"); do
      systemd-run --quiet --unit=senpai-$n python3 "$BIN/senpai.py" "$APPCG/app-$n-1.service"
      HELPERS="$HELPERS /sys/fs/cgroup/system.slice/senpai-$n.service"
    done
    sleep 2; systemctl is-active senpai-browser || journalctl -u senpai-browser --no-pager | tail -5
    DCG= ;;
  *) DCG= ;;
esac

T0=$(date +%s)
el() { echo $(( $(date +%s) - T0 )); }
snap 0 >> "$OUT/samples.csv"
# Optional game phase: $BIN/game.conf = "<start_s> <base_mb> <level_mb> <level_at_s> <threads>".
# The game runs as app-gamescope-1 (protected by Ultra RAM's default list) until the run ends.
GAME_CONF=$( [ -f "$BIN/game.conf" ] && cat "$BIN/game.conf" )
GAME_STARTED=
while [ "$(el)" -lt "$RUN_SECS" ]; do
  sleep 10; snap "$(el)" >> "$OUT/samples.csv"
  if [ -n "$GAME_CONF" ] && [ -z "$GAME_STARTED" ] && [ "$(el)" -ge "$(awk '{print $1}' <<< "$GAME_CONF")" ]; then
    read -r _ gb gl gat gth <<< "$GAME_CONF"
    echo "game starts at $(el)s: $gb MB + level $gl MB at ${gat}s, $gth threads"
    echo "game_start=$(snap x)" >> "$OUT/phase.txt"; echo "game_t0=$(el)" >> "$OUT/phase.txt"
    asu systemd-run --user --quiet --unit=app-gamescope-1 /usr/local/bin/fakegame "$gb" "$gl" "$gat" "$gth" "$RES/game.txt"
    GAME_STARTED=1
  fi
done
if [ -n "$GAME_STARTED" ]; then
  asu systemctl --user kill -s SIGUSR1 app-gamescope-1.service
  for _ in $(seq 600); do [ -f "$RES/game.txt" ] && break; sleep 1; done
  echo "game: $(cat $RES/game.txt)"; cp "$RES/game.txt" "$OUT/"
  echo "game_end=$(snap x)" >> "$OUT/phase.txt"; echo "game_t1=$(el)" >> "$OUT/phase.txt"
  sleep 5
fi

# ── phase 2: open a new 900 MB app while the old ones are still running ──
echo "launching newapp at $(el)s"
B=$(snap x)
start_app newapp 900 20 20
for _ in $(seq 1800); do [ -f "$RES/newapp.txt.ready" ] && break; sleep 1; snap "$(el)" >> "$OUT/samples.csv"; done
echo "newapp $(cat $RES/newapp.txt.ready)"
A=$(snap x)
echo "phase2_before=$B" >> "$OUT/phase.txt"; echo "phase2_after=$A" >> "$OUT/phase.txt"
T2=$(date +%s); while [ $(( $(date +%s) - T2 )) -lt 120 ]; do sleep 10; snap "$(el)" >> "$OUT/samples.csv"; done

# ── daemon cost ──
if [ -n "$DCG" ] && [ -r "$DCG/cpu.stat" ]; then
  echo "daemon_cpu_usec=$(awk '/usage_usec/{print $2}' "$DCG/cpu.stat")" >> "$OUT/phase.txt"
  echo "daemon_mem_peak=$(cat "$DCG/memory.peak" 2>/dev/null)" >> "$OUT/phase.txt"
fi
hc=0; for h in ${HELPERS:-}; do hc=$((hc + $(awk '/usage_usec/{print $2}' "$h/cpu.stat" 2>/dev/null || echo 0))); done
echo "helper_cpu_usec=$hc" >> "$OUT/phase.txt"
case $SC in
  wolfram) cp /var/lib/wolfram/stats.json "$OUT/" 2>/dev/null; journalctl -u wolfram --no-pager > "$OUT/daemon.log" ;;
  ultra-ram*) /usr/local/bin/ultra-ram zram-tier status > "$OUT/tier-status.txt" 2>&1; asu /usr/local/bin/ultra-ram status > "$OUT/daemon-status.txt" 2>&1; journalctl _SYSTEMD_USER_UNIT=ultra-ram.service --no-pager > "$OUT/daemon.log" ;;
esac
# Freeze the daemon before the switch-back test so it measures the state it left behind.
case $SC in wolfram) systemctl kill -s SIGSTOP wolfram ;; ultra-ram*) asu systemctl --user kill -s SIGSTOP ultra-ram.service ;; esac
snap end-run >> "$OUT/phase.txt"

# ── phase 3: close the new app, then switch back to every background app, one at a time ──
for n in newapp browser chat launcher editor mail; do
  asu systemctl --user kill -s SIGUSR1 "app-$n-1.service"
  for _ in $(seq 1800); do [ -f "$RES/$n.txt" ] && break; sleep 1; done
done
cp $RES/*.txt "$OUT/"
hc=0; for h in ${HELPERS:-}; do hc=$((hc + $(awk '/usage_usec/{print $2}' "$h/cpu.stat" 2>/dev/null || echo 0))); done
echo "helper_cpu_end_usec=$hc" >> "$OUT/phase.txt"
journalctl -u uram-prefetch -u uram-maint -u uram-age -u 'senpai-*' --no-pager > "$OUT/helpers.log" 2>/dev/null
/usr/local/bin/ultra-ram zram-tier status > "$OUT/tier-status-end.txt" 2>&1
snap end >> "$OUT/phase.txt"
echo "== done $SC"
