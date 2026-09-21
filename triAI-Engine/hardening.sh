#!/usr/bin/env bash
set -euo pipefail

# ═══════════════════════════════════════════════════════════════
# PHASE 3: SYSTEM-HYGIENE
# Haertet das Betriebssystem fuer den triAI-Engine Betrieb.
# Ausfuehrung: sudo bash hardening.sh
# ═══════════════════════════════════════════════════════════════

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PRIMARY_MODEL_PID=""

echo "═══ triAI-Engine System-Haertung ═══"
echo "Startzeit: $(date -Iseconds)"
echo ""

# ────────────────────────────────────────────────────────────
# 1. ZRAM Aktivierung (Komprimierter RAM als Swap-Puffer)
# ────────────────────────────────────────────────────────────
echo "[1/6] Konfiguriere ZRAM..."

if command -v zramctl &>/dev/null; then
    ZRAM_SIZE=$(awk '/MemTotal/ {printf "%d", $2/2/1024}' /proc/meminfo)
    zramctl --find --size "${ZRAM_SIZE}M" -o 80
    mkswap "/dev/zram0" 2>/dev/null || true
    swapon "/dev/zram0" 2>/dev/null || true
    echo "ZRAM aktiviert: ${ZRAM_SIZE}M komprimierter Swap"
else
    echo "WARNING: zramctl nicht gefunden. ZRAM manuell konfigurieren."
    # Fallback: ZRAM ueber Kernel-Modul
    if [ -f /sys/class/zram-control ]; then
        echo "ZRAM-Control vorhanden. Konfiguration manuell erfordert."
    fi
fi

# ────────────────────────────────────────────────────────────
# 2. OOM-Schutz: OOM-Score fuer AI-Prozesse senken
# ────────────────────────────────────────────────────────────
echo "[2/6] Konfiguriere OOM-Schutz..."

AI_PIDS=$(pgrep -f "tri-ai-engine|llama-server|huxxel" 2>/dev/null || true)
if [ -n "$AI_PIDS" ]; then
    for PID in $AI_PIDS; do
        if [ -f "/proc/$PID/oom_score_adj" ]; then
            echo -1000 > "/proc/$PID/oom_score_adj" 2>/dev/null || true
            echo "OOM-Score fuer PID $PID auf -1000 gesetzt"
        fi
    done
else
    echo "Keine AI-Prozesse gefunden. OOM-Score beim naechsten Start konfigurieren."
fi

# OOM-Schwellenwert anpassen
CURRENT_OOM=$(cat /proc/sys/vm/overcommit_memory 2>/dev/null || echo "3")
echo "vm.overcommit_memory = $CURRENT_OOM"
sysctl -w vm.overcommit_memory=2 2>/dev/null || true

# ────────────────────────────────────────────────────────────
# 3. Swappiness reduzieren
# ────────────────────────────────────────────────────────────
echo "[3/6] Reduziere Swappiness..."
CURRENT_SWAPPINESS=$(cat /proc/sys/vm/swappiness 2>/dev/null || echo "60")
sysctl -w vm.swappiness=10 2>/dev/null || true
echo "Swappiness von $CURRENT_SWAPPINESS auf 10 gesetzt"

# ────────────────────────────────────────────────────────────
# 4. CPU Governor auf Performance
# ────────────────────────────────────────────────────────────
echo "[4/6] Setze CPU Governor auf Performance..."
if command -v cpupower &>/dev/null; then
    cpupower frequency-set -g performance 2>/dev/null || true
    echo "CPU Governor auf Performance gesetzt via cpupower"
elif command -v cpufreq-set &>/dev/null; then
    for CPUFREQ_PATH in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
        if [ -w "$CPUFREQ_PATH" ]; then
            echo performance > "$CPUFREQ_PATH" 2>/dev/null || true
        fi
    done
    echo "CPU Governor auf Performance gesetzt via cpufreq-set"
else
    echo "WARNING: Kein CPU-Frequenz-Tool gefunden. Manuell setzen:"
    echo "  echo performance | tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor"
fi

# ────────────────────────────────────────────────────────────
# 5. Kernel-Parameter fuer Inferenz-Optimierung
# ────────────────────────────────────────────────────────────
echo "[5/6] Optimiere Kernel-Parameter..."

# HugePages konfigurieren
HUGE_PAGES=$(grep -c HugePages_Total /proc/meminfo 2>/dev/null || echo "0")
echo "HugePages konfiguriert: $(cat /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages 2>/dev/null || echo 'nicht gesetzt')"
sysctl -w vm.nr_hugepages=512 2>/dev/null || true

# I/O Scheduler auf noop/mq-deadline fuer SSD optimieren
for DISK in /sys/block/nvme*/queue/scheduler /sys/block/sd*/queue/scheduler; do
    if [ -w "$DISK" ]; then
        echo "mq-deadline" > "$DISK" 2>/dev/null || true
    fi
done

# Tempfs fuers /tmp (reduziert Disk-IO)
mount -t tmpfs -o size=4G tmpfs /tmp 2>/dev/null || true

# ────────────────────────────────────────────────────────────
# 6. Device-Node pruefen
# ────────────────────────────────────────────────────────────
echo "[6/6] Pruefe Kernel-Device..."
if [ -c /dev/tri_ai_worker ]; then
    echo "Kernel-Device /dev/tri_ai_worker gefunden!"
    chmod 666 /dev/tri_ai_worker 2>/dev/null || true
else
    echo "WARNING: Kernel-Device /dev/tri_ai_worker nicht gefunden."
    echo "Lade das Kernel-Modul: cd kernel && make && sudo make load"
fi

echo ""
echo "═══ Haertung abgeschlossen ═══"
echo "Beendet: $(date -Iseconds)"
echo ""
echo "Naechster Schritt: ./stress_test.sh"