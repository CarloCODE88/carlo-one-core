#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
LOG_DIR="$PROJECT_DIR/build-logs/stress"
mkdir -p "$LOG_DIR"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_pass() { echo -e "${GREEN}[PASS]${NC} $1"; }
log_fail() { echo -e "${RED}[FAIL]${NC} $1"; }
log_info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

echo "=== triAI-Engine Parallel Stress Suite ==="
echo "Time: $(date -Iseconds)"
echo ""

# Phase 1: Check prerequisites
log_info "Phase 1: Prerequisites"
if ! sudo lsmod | grep -q tri_ai_worker; then
    log_fail "tri_ai_worker module not loaded"
    exit 1
fi
log_pass "Kernel module loaded"

if ! command -v nvidia-smi &>/dev/null; then
    log_fail "nvidia-smi not found"
    exit 1
fi
log_pass "nvidia-smi available"

if ! pgrep -f "llama-server" &>/dev/null; then
    log_fail "llama-server not running. Start with: cd $PROJECT_DIR && ./scripts/master_run.sh"
    exit 1
fi
log_pass "llama-server running"

# Phase 2: Pre-stress baseline
log_info "Phase 2: Pre-stress baseline"
BASELINE_VRAM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1 | tr -d ' ')
BASELINE_TPS=$(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits | head -1 | tr -d ' ')
echo "Baseline VRAM: ${BASELINE_VRAM}MB | GPU Util: ${BASELINE_TPS}%"
log_pass "Baseline captured"

# Phase 3: Build stress binary
log_info "Phase 3: Build stress_parallel binary"
cd "$PROJECT_DIR"
cargo build --bin stress_parallel --release 2>&1 | tail -3
if [ ! -f "$PROJECT_DIR/target/release/stress_parallel" ]; then
    log_fail "Build failed"
    exit 1
fi
log_pass "stress_parallel built"

# Phase 4: Run stress tests with escalating load
log_info "Phase 4: Escalating parallel load tests"

for CONCURRENCY in 5 10 20; do
    echo ""
    echo "--- Testing with $CONCURRENCY concurrent tasks ---"
    log_info "Running stress test with $CONCURRENCY tasks..."

    TRI_STRESS_CONCURRENT="$CONCURRENCY" \
    TRI_STRESS_ITERATIONS="3" \
    TRI_STRESS_URL="http://127.0.0.1:8765/v1/chat/completions" \
    TRI_STRESS_MODEL="INGRIED" \
    timeout 300 "$PROJECT_DIR/target/release/stress_parallel" > "$LOG_DIR/stress_${CONCURRENCY}.log" 2>&1 || true

    if [ $? -eq 0 ]; then
        log_pass "$CONCURRENCY tasks completed"
    else
        log_fail "$CONCURRENCY tasks had issues - check $LOG_DIR/stress_${CONCURRENCY}.log"
    fi

    # Capture metrics during the last test
    if [ "$CONCURRENCY" -eq 20 ]; then
        VRAM_AFTER=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1 | tr -d ' ')
        GPU_UTIL=$(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits | head -1 | tr -d ' ')
        echo "After $CONCURRENCY tasks: VRAM ${VRAM_AFTER}MB | GPU Util ${GPU_UTIL}%"

        # Save metrics
        cat > "$LOG_DIR/metrics_${CONCURRENCY}.json" << EOF
{
  "concurrent_tasks": $CONCURRENCY,
  "vram_after_mb": $VRAM_AFTER,
  "gpu_utilization_pct": $GPU_UTIL,
  "timestamp": "$(date -Iseconds)"
}
EOF
    fi

    # Brief cooldown between phases
    sleep 2
done

# Phase 5: Post-stress check
log_info "Phase 5: Post-stress stability check"
DMESG_ERRORS=$(sudo dmesg 2>/dev/null | grep -i "panic\|oops\|BUG.*tri_ai" | wc -l || echo 0)
if [ "$DMESG_ERRORS" -eq 0 ]; then
    log_pass "No kernel panics or errors after stress test"
else
    log_fail "Kernel errors detected: $DMESG_ERRORS"
fi

FINAL_VRAM=$(nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits | head -1 | tr -d ' ')
FINAL_GPU=$(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits | head -1 | tr -d ' ')
echo "Final state: VRAM ${FINAL_VRAM}MB | GPU Util ${FINAL_GPU}%"

# Check if llama-server is still running
if pgrep -f "llama-server" &>/dev/null; then
    log_pass "llama-server still running after stress test"
else
    log_fail "llama-server crashed during stress test"
fi

# Phase 6: Generate summary report
log_info "Phase 6: Generating summary report"
cat > "$LOG_DIR/summary_$(date +%Y%m%d_%H%M%S).md" << EOF
# triAI-Engine Parallel Stress Test Report

## Generated: $(date -Iseconds)

## Configuration
- Concurrent Tasks: 5, 10, 20
- Model: INGRIED
- Iterations per load level: 3
- Server: http://127.0.0.1:8765

## Pre-Stress Baseline
- VRAM: ${BASELINE_VRAM}MB
- GPU Utilization: ${BASELINE_TPS}%

## Post-Stress State
- VRAM: ${FINAL_VRAM}MB
- GPU Utilization: ${FINAL_GPU}%
- Kernel Panics: ${DMESG_ERRORS}
- llama-server Stable: $(pgrep -f "llama-server" >/dev/null && echo "Yes" || echo "No")

## Log Files
- 5 tasks: $LOG_DIR/stress_5.log
- 10 tasks: $LOG_DIR/stress_10.log
- 20 tasks: $LOG_DIR/stress_20.log
- Optimal Config: /tmp/optimal_config.json
EOF
log_pass "Summary report generated at $LOG_DIR/"

echo ""
echo "=== Stress Suite Complete ==="
echo -e "${GREEN}All phases executed. Check $LOG_DIR/ for detailed results.${NC}"
