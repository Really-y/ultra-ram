#!/usr/bin/env bash
# ultra-ram quraşdırıcı. Root lazım deyil — hər şey istifadəçi səviyyəsindədir.
#   ./install.sh                   qur və işə sal
#   ./install.sh --with-zram-tier  üstəlik istəyə bağlı zram tier sistem xidmətini qur (sudo ilə)
#   ./install.sh --with-prefetch   üstəlik istəyə bağlı prefetch sistem xidmətini qur (sudo ilə)
#   ./install.sh --with-age        üstəlik istəyə bağlı MGLRU yaşlandırma sistem xidmətini qur (sudo ilə)
#   ./install.sh --with-root       hamısı: --with-zram-tier --with-prefetch --with-age
#   ./install.sh --server          server profile instead: root system service ultra-ram-server.service
#                                  (services, containers, VMs, pods); combine with --with-root etc.
#   ./install.sh --uninstall       hər şeyi geri qaytar və sil (server profile included)
set -euo pipefail
cd "$(dirname "$(readlink -f "$0")")"

WITH_TIER=0
WITH_PREFETCH=0
WITH_AGE=0
UNINSTALL=0
SERVER=0
for a in "$@"; do
    case "$a" in
        --with-zram-tier) WITH_TIER=1 ;;
        --with-prefetch) WITH_PREFETCH=1 ;;
        --with-age) WITH_AGE=1 ;;
        --with-root) WITH_TIER=1; WITH_PREFETCH=1; WITH_AGE=1 ;;
        --server) SERVER=1 ;;
        --uninstall) UNINSTALL=1 ;;
        *) echo "✗ Naməlum parametr: $a (mümkün: --with-zram-tier, --with-prefetch, --with-age, --with-root, --server, --uninstall)"; exit 2 ;;
    esac
done

# Root olaraq işə salınıbsa sudo lazım deyil (minimal serverlərdə sudo olmaya bilər).
if [[ $EUID == 0 ]]; then SUDO=""; else SUDO="sudo"; fi

# İstəyə bağlı root hissələri (zram tier, prefetch, age): ayrıca sistem xidmətləri, ortaq binar.
TIER_BIN="/usr/local/bin/ultra-ram"
TIER_UNIT="/etc/systemd/system/ultra-ram-zram-tier.service"
PREFETCH_UNIT="/etc/systemd/system/ultra-ram-prefetch.service"
AGE_UNIT="/etc/systemd/system/ultra-ram-age.service"
# Server profile: the same binary as a root system service.
SERVER_UNIT="/etc/systemd/system/ultra-ram-server.service"
SERVER_CFG="/etc/ultra-ram"
# The root helpers work on the server workloads when installed with --server.
HELPER_ARGS=""
[[ $SERVER == 1 ]] && HELPER_ARGS=" --profile server"
# Server: age every 150 s, so the oldest generation is >= 300 s idle (the server cold_age_secs).
AGE_ARGS="$HELPER_ARGS"
[[ $SERVER == 1 ]] && AGE_ARGS="$HELPER_ARGS --interval-secs 150"

BIN="$HOME/.local/bin/ultra-ram"
UNIT="$HOME/.config/systemd/user/ultra-ram.service"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/ultra-ram"
RUN="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ultra-ram"

if [[ $UNINSTALL == 1 ]]; then
    ROOT_PARTS=0
    if [[ -f "$SERVER_UNIT" ]]; then
        # Stopping removes /run/ultra-ram (RuntimeDirectory). Offloaded pages flow back on use.
        $SUDO systemctl disable --now ultra-ram-server.service || true
        $SUDO rm -f "$SERVER_UNIT"
        ROOT_PARTS=1
        echo "✓ server profile service removed. Config kept: $SERVER_CFG  (to delete: sudo rm -r $SERVER_CFG)"
    fi
    if [[ -f "$PREFETCH_UNIT" ]]; then
        $SUDO systemctl disable --now ultra-ram-prefetch.service || true
        $SUDO rm -f "$PREFETCH_UNIT"
        ROOT_PARTS=1
        echo "✓ prefetch xidməti silindi."
    fi
    if [[ -f "$AGE_UNIT" ]]; then
        # Dayananda systemd /run/ultra-ram-age-i (RuntimeDirectory) silir: daemon köhnə rejimə qayıdır.
        $SUDO systemctl disable --now ultra-ram-age.service || true
        $SUDO rm -f "$AGE_UNIT"
        ROOT_PARTS=1
        echo "✓ age xidməti silindi."
    fi
    if [[ -f "$TIER_UNIT" ]]; then
        # Dayandırmaq ExecStopPost ilə tier-i silir (swapoff + hot_remove + zswap bərpası).
        $SUDO systemctl disable --now ultra-ram-zram-tier.service || true
        if [[ -e /run/ultra-ram-zram-tier ]]; then
            # remove imtina edib (RAM çatmır) və ya ümumiyyətlə işləməyib: binar və unit lazımdır.
            [[ $ROOT_PARTS == 1 ]] && $SUDO systemctl daemon-reload
            echo "✗ zram tier hələ aktivdir (çox güman ki, onun içindəkiləri geri qaytarmağa RAM çatmır)."
            echo "  $TIER_UNIT və $TIER_BIN saxlanıldı, başqa heç nə silinmədi."
            echo "  Yaddaş boşaldıqdan sonra:  sudo $TIER_BIN zram-tier remove  &&  $0 --uninstall"
            echo "  Vəziyyət:  $TIER_BIN zram-tier status"
            exit 1
        fi
        $SUDO rm -f "$TIER_UNIT"
        ROOT_PARTS=1
        echo "✓ zram tier xidməti silindi."
    fi
    if [[ $ROOT_PARTS == 1 ]]; then
        $SUDO rm -f "$TIER_BIN"
        $SUDO systemctl daemon-reload
    fi
    # The desktop part, if this user has it (a server-only install has no user unit).
    if [[ -f "$UNIT" || -x "$BIN" ]]; then
        if [[ -x "$BIN" ]]; then "$BIN" rollback || true; fi
        systemctl --user disable --now ultra-ram.service 2>/dev/null || true
        rm -f "$UNIT" "$BIN"
        systemctl --user daemon-reload || true
        rm -rf "$RUN"
        echo "✓ ultra-ram silindi. Ayarlar saxlanıldı: $CFG  (silmək üçün: rm -r \"$CFG\")"
    fi
    exit 0
fi

if [[ $SERVER == 1 ]]; then
    # ── Server profile: compatibility checks first, nothing is installed if they fail ──
    [[ "$(stat -fc %T /sys/fs/cgroup)" == "cgroup2fs" ]] || { echo "✗ cgroup v2 is required."; exit 1; }
    [[ -r /proc/pressure/memory ]] || { echo "✗ The kernel has no PSI (/proc/pressure/memory)."; exit 1; }
    [[ -e /sys/fs/cgroup/system.slice/memory.reclaim ]] || { echo "✗ memory.reclaim is missing (Linux >= 5.19 with the memory controller enabled for system.slice)."; exit 1; }
    if ! swapon --noheadings 2>/dev/null | grep -q .; then
        echo "! Warning: no swap or zram is active. The service is installed but offloads nothing without swap."
    fi
    cargo build --release
    $SUDO install -Dm755 target/release/ultra-ram "$TIER_BIN"
    $SUDO mkdir -p "$SERVER_CFG"
    if [[ ! -f "$SERVER_CFG/config" ]]; then
        "$TIER_BIN" print-default-config --profile server | $SUDO tee "$SERVER_CFG/config" >/dev/null
    elif ! grep -Eq '^[[:space:]]*profile[[:space:]]*=[[:space:]]*server' "$SERVER_CFG/config"; then
        echo "! $SERVER_CFG/config exists without 'profile = server'; kept as is (the unit passes --profile server)."
    fi
    $SUDO tee "$SERVER_UNIT" >/dev/null <<'EOF'
[Unit]
Description=Ultra RAM server profile — proactive cold-memory offload for services, containers and pods
Documentation=https://github.com/Really-y/ultra-ram
After=swap.target
StartLimitIntervalSec=900
StartLimitBurst=3

[Service]
Type=simple
ExecStart=/usr/local/bin/ultra-ram daemon --profile server
Restart=on-failure
RestartSec=30
# The daemon itself must not become the problem. Reclaim runs in its context, so it keeps a
# normal I/O class (idle I/O could stall swap writes on a busy disk) but a low CPU share.
Nice=10
CPUWeight=20
IOSchedulingClass=best-effort
IOSchedulingPriority=7
MemoryMax=64M
# Root, but no capability: memory.reclaim of system services, containers and pods is owned by
# root (0644), so the owner permission is enough — no CAP_DAC_OVERRIDE. The kernel has no
# capability check on memory.reclaim (no CAP_SYS_ADMIN); the only restriction is cgroup-namespace
# delegation, so ProtectControlGroups=, PrivateNetwork= (a fresh /sys without the cgroup tree)
# and ProtectKernelTunables= (read-only /sys) must stay off. Cgroups chowned to another user
# (Delegate= with User=) are not writable this way and are simply skipped by the kernel (EACCES).
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
# /run/ultra-ram/status.json (+ status.txt, history), readable by everyone.
RuntimeDirectory=ultra-ram
RuntimeDirectoryMode=0755

[Install]
WantedBy=multi-user.target
EOF
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable ultra-ram-server.service
    # State is only counters and the learned ceilings: restarting with the new binary is safe.
    $SUDO systemctl restart ultra-ram-server.service
    echo "✓ server profile active:  ultra-ram status --profile server  |  ultra-ram status --json  |  journalctl -u ultra-ram-server -f"
    echo "  config: $SERVER_CFG/config (reloaded every minute)"
fi

if [[ $SERVER == 0 ]]; then
# ── Uyğunluq yoxlamaları: sistem uyğun deyilsə heç nə qurulmur ──
APP="/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/app.slice"
[[ "$(stat -fc %T /sys/fs/cgroup)" == "cgroup2fs" ]] || { echo "✗ cgroup v2 lazımdır."; exit 1; }
[[ -r /proc/pressure/memory ]] || { echo "✗ Kernel PSI dəstəkləmir (/proc/pressure/memory yoxdur)."; exit 1; }
[[ -w "$APP/memory.reclaim" ]] || { echo "✗ $APP/memory.reclaim yazıla bilmir (kernel ≥ 5.19 və systemd istifadəçi sessiyası lazımdır)."; exit 1; }
if ! swapon --noheadings 2>/dev/null | grep -q .; then
    echo "! Xəbərdarlıq: swap/zram aktiv deyil. Daemon qurulacaq, amma swap olmadan heç nə köçürməyəcək."
fi

cargo build --release
install -Dm755 target/release/ultra-ram "$BIN"

mkdir -p "$CFG"
[[ -f "$CFG/config" ]] || "$BIN" print-default-config > "$CFG/config"

mkdir -p "$(dirname "$UNIT")"
cat > "$UNIT" <<'EOF'
[Unit]
Description=Ultra RAM — PSI əsaslı ağıllı yaddaş idarəetməsi

[Service]
ExecStart=%h/.local/bin/ultra-ram daemon
Restart=on-failure
RestartSec=30
# Daemon özü sistemə yük olmamalıdır.
Slice=background.slice
Nice=19
IOSchedulingClass=idle
MemoryMax=64M
NoNewPrivileges=yes

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable ultra-ram.service
systemctl --user restart ultra-ram.service
fi

if [[ $WITH_TIER == 1 ]]; then
    echo
    echo "• zram tier qurulur (sudo lazımdır)…"
    $SUDO install -Dm755 target/release/ultra-ram "$TIER_BIN"
    $SUDO tee "$TIER_UNIT" >/dev/null <<'EOF'
[Unit]
Description=Ultra RAM zram tier — lz4 zram, boş səhifələr zstd ilə yenidən sıxılır
# Distributivin öz zram0-ından sonra başla ki, prioritetimiz ondan yüksək olsun.
After=swap.target systemd-zram-setup@zram0.service
# Quraşdırma alınmırsa sonsuz təkrar cəhd etməsin.
StartLimitIntervalSec=900
StartLimitBurst=3

[Service]
Type=simple
ExecStartPre=/usr/local/bin/ultra-ram zram-tier setup
ExecStart=/usr/local/bin/ultra-ram zram-tier maintain --loop 300 --idle-secs 1800
# Tier yalnız təmiz, açıq dayandırmada (systemctl stop) silinir: systemd öz SIGTERM-i ilə
# dayanmanı SERVICE_RESULT=success sayır. Çökmə, OOM və ya xəta ilə çıxışda tier yerində qalır
# (Restart onu yenidən götürür). Sistem söndürülərkən də silinmir: swapoff gigabaytları RAM-a
# çəkərdi və söndürməni ləngidərdi. $$ — systemd üçün hərfi $.
ExecStopPost=/bin/sh -c '[ "$$SERVICE_RESULT" = success ] && [ "$$(systemctl is-system-running)" != stopping ] && exec /usr/local/bin/ultra-ram zram-tier remove || true'
# Dolu tier-in swapoff-u dəqiqələr çəkə bilər.
TimeoutStopSec=5min
Restart=on-failure
RestartSec=60
# Yenidən sıxma (zstd) kernel-də bu prosesin adından işləyir. CPUSchedulingPolicy=idle yox:
# recompress zram slot kilidlərini tutur, idle sinifli axın onları tutarkən tətbiqlərin swap-in-i
# gözləyə bilər.
Nice=19
IOSchedulingClass=idle
# Lazımdır: CAP_SYS_ADMIN (swapon/swapoff), yazıla bilən /sys və /run, mkswap, modprobe.
ProtectHome=yes
PrivateTmp=yes

[Install]
WantedBy=multi-user.target
EOF
    $SUDO systemctl daemon-reload
    # restart yox: işləyən tier-i yenidən başlatmaq onu swapoff edərdi (ExecStopPost=remove).
    $SUDO systemctl enable --now ultra-ram-zram-tier.service
    echo "✓ zram tier aktivdir:  ultra-ram zram-tier status"
fi

if [[ $WITH_PREFETCH == 1 ]]; then
    echo
    echo "• prefetch xidməti qurulur (sudo lazımdır)…"
    $SUDO install -Dm755 target/release/ultra-ram "$TIER_BIN"
    # @HELPER_ARGS@: " --profile server" with --server, empty otherwise.
    sed "s|@HELPER_ARGS@|$HELPER_ARGS|" <<'EOF' | $SUDO tee "$PREFETCH_UNIT" >/dev/null
[Unit]
Description=Ultra RAM prefetch — tətbiqə qayıdanda zram-dakı yaddaşını qabaqcadan RAM-a qaytarır
After=swap.target
StartLimitIntervalSec=900
StartLimitBurst=3

[Service]
Type=simple
ExecStart=/usr/local/bin/ultra-ram prefetch run@HELPER_ARGS@
Restart=on-failure
RestartSec=30
# 3 = kernel/arxitektura dəstəkləmir: yenidən başlatmaq kömək etməz.
RestartPreventExitStatus=3
# Nice yoxdur: istifadəçi tətbiqi gözləyərkən işləyir, cəld olmalıdır. Rəqabətdə payı yarımdır.
CPUWeight=50
# CAP_SYS_NICE: process_madvise; CAP_SYS_PTRACE: başqa istifadəçilərin /proc/PID/smaps-ı və
# process_madvise-ın ptrace-read yoxlaması. /proc və /sys ProtectSystem=strict-dən azaddır.
CapabilityBoundingSet=CAP_SYS_NICE CAP_SYS_PTRACE CAP_DAC_READ_SEARCH
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes
# Reclaim daemon üçün marker faylları (/run/ultra-ram-prefetch/*), hamı oxuya bilər.
RuntimeDirectory=ultra-ram-prefetch
RuntimeDirectoryMode=0755

[Install]
WantedBy=multi-user.target
EOF
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable ultra-ram-prefetch.service
    # Vəziyyəti yoxdur — yeni binarla yenidən başlatmaq təhlükəsizdir.
    $SUDO systemctl restart ultra-ram-prefetch.service
    echo "✓ prefetch aktivdir:  journalctl -u ultra-ram-prefetch -f"
fi

if [[ $WITH_AGE == 1 ]]; then
    echo
    echo "• age (MGLRU yaşlandırma) xidməti qurulur (sudo lazımdır)…"
    if [[ ! -e /sys/kernel/mm/lru_gen/enabled ]]; then
        echo "! Xəbərdarlıq: kernel MGLRU-suzdur (/sys/kernel/mm/lru_gen yoxdur) — xidmət işə düşməyəcək."
    fi
    $SUDO install -Dm755 target/release/ultra-ram "$TIER_BIN"
    sed "s|@AGE_ARGS@|$AGE_ARGS|" <<'EOF' | $SUDO tee "$AGE_UNIT" >/dev/null
[Unit]
Description=Ultra RAM age — tətbiqlərin yaddaşını MGLRU ilə mütəmadi yaşlandırır (soyuq yaddaş xəritəsi)
# /sys/kernel/debug/lru_gen lazımdır.
Wants=sys-kernel-debug.mount
After=sys-kernel-debug.mount
StartLimitIntervalSec=900
StartLimitBurst=3

[Service]
Type=simple
ExecStart=/usr/local/bin/ultra-ram age run@AGE_ARGS@
Restart=on-failure
RestartSec=30
# 3 = MGLRU/debugfs yoxdur, kernel lockdown və s.: yenidən başlatmaq kömək etməz.
RestartPreventExitStatus=3
# Səhifə cədvəli gəzintisi bu prosesin adından kernel-də işləyir; tələsik deyil.
Nice=10
IOSchedulingClass=idle
# Root, amma heç bir capability lazım deyil: debugfs (0700 root) və lru_gen (0644 root) sahiblik
# hüququ ilə açılır, cgroup faylları hamıya oxunandır.
CapabilityBoundingSet=
NoNewPrivileges=yes
# strict /sys-i yazıla bilən saxlayır (lru_gen-ə yazmaq lazımdır). ProtectKernelTunables=yes OLMAZ:
# /sys/kernel/debug-ı yalnız oxunan edir (EROFS). PrivateNetwork=yes də OLMAZ: /sys-in yerinə
# təzə sysfs qoşur, orada nə debugfs, nə də /sys/fs/cgroup olur.
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectControlGroups=yes
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
# Reclaim daemon üçün xəritə faylları (/run/ultra-ram-age/*), hamı oxuya bilər.
RuntimeDirectory=ultra-ram-age
RuntimeDirectoryMode=0755

[Install]
WantedBy=multi-user.target
EOF
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable ultra-ram-age.service
    # Vəziyyəti kernel-dədir — yeni binarla yenidən başlatmaq təhlükəsizdir.
    $SUDO systemctl restart ultra-ram-age.service
    echo "✓ age aktivdir:  journalctl -u ultra-ram-age -f  |  sudo ultra-ram age status"
fi

if [[ $SERVER == 1 ]]; then
    echo
    echo "✓ Installed and running (server profile)."
    exit 0
fi

echo
echo "✓ Quruldu və işə düşdü."
echo "  ultra-ram analyze    dərin analiz"
echo "  ultra-ram watch      canlı izləmə"
echo "  ultra-ram status     daemon nə edib"
echo "  ultra-ram rollback   hər şeyi geri qaytar"
case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "! $HOME/.local/bin PATH-da yoxdur — tam yolla çağırın: $BIN" ;; esac
