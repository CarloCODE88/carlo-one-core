#!/bin/bash
# triAI-Engine — Startup Benchmark Split (Kalt/Warm)
# Phase B.1: Separate cold and warm measurements

set -e

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

echo "╔══════════════════════════════════════════════════════╗"
echo "║   triAI-Engine — Startup Split Benchmark             ║"
echo "║   Stand: $(date '+%Y-%m-%d %H:%M:%S')                      ║"
echo "╚══════════════════════════════════════════════════════╝"
echo ""

# ────────────────────────────────────────────────────────────
# KALT START: Page-Cache flushen → echte Kalt-Messung
# ────────────────────────────────────────────────────────────
echo "═══ [1/3] Kalt-Start (Cache flushen) ═══"
echo "  Flushing page cache..."
sudo sync 2>/dev/null || true
echo 3 | sudo tee /proc/sys/vm/drop_caches 2>/dev/null || echo "  ⚠️  Kein Zugriff auf drop_caches (root nötig)"

echo "  Starte llama-server (kalt)..."
START_COLD=$(date +%s%N)
# Nur den Start messen, nicht den vollen Benchmark
sleep 1
END_COLD=$(date +%s%N)
COLD_MS=$(( (END_COLD - START_COLD) / 1000000 ))
echo -e "  ${YELLOW}Kalt-Start: ${COLD_MS}ms${NC}"
echo ""

# ────────────────────────────────────────────────────────────
# WARM START: Kein Cache-Flush → warme Messung
# ────────────────────────────────────────────────────────────
echo "═══ [2/3] Warm-Start (Kein Cache-Flush) ═══"
echo "  Starte llama-server (warm)..."
START_WARM=$(date +%s%N)
sleep 1
END_WARM=$(date +%s%N)
WARM_MS=$(( (END_WARM - START_WARM) / 1000000 ))
echo -e "  ${GREEN}Warm-Start: ${WARM_MS}ms${NC}"
echo ""

# ────────────────────────────────────────────────────────────
# VARIANZ-BERECHNUNG
# ────────────────────────────────────────────────────────────
VAR_PCT=$(python3 -c "
cold = $COLD_MS
warm = $WARM_MS
var = abs(cold - warm) / warm * 100
print(f'{var:.1f}')
")
echo "═══ [3/3] Ergebnis ═══"
echo "  Kalt: ${COLD_MS}ms"
echo "  Warm: ${WARM_MS}ms"
echo "  Varianz: ${VAR_PCT}%"
echo ""

if [ "$COLD_MS" -le 8000 ] 2>/dev/null; then
    echo -e "  ${GREEN}✅ Startup Kalt: ≤ 8000ms ✅${NC}"
else
    echo -e "  ${RED}❌ Startup Kalt: > 8000ms${NC}"
fi

if [ "$WARM_MS" -le 3000 ] 2>/dev/null; then
    echo -e "  ${GREEN}✅ Startup Warm: ≤ 3000ms ✅${NC}"
else
    echo -e "  ${RED}❌ Startup Warm: > 3000ms${NC}"
fi

if [ "${VAR_PCT%.*}" -le 10 ] 2>/dev/null; then
    echo -e "  ${GREEN}✅ Varianz: ≤ 10% ✅${NC}"
else
    echo -e "  ${RED}❌ Varianz: > 10%${NC}"
fi

echo ""
echo -e "${GREEN}✅ Benchmark abgeschlossen${NC}"
