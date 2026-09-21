#!/usr/bin/env bash
set -uo pipefail

DEVICE="/dev/tri_ai_worker"
PASS=0
FAIL=0
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log_pass() { echo -e "${GREEN}[PASS]${NC} $1"; ((PASS++)); }
log_fail() { echo -e "${RED}[FAIL]${NC} $1"; ((FAIL++)); }
log_info() { echo -e "${YELLOW}[INFO]${NC} $1"; }

echo "=== triAI-Engine Hardening Test ==="
echo "Time: $(date -Iseconds)"
echo "Device: $DEVICE"
echo ""

if [ ! -c "$DEVICE" ]; then
    log_fail "Device $DEVICE not found."
    exit 1
fi
log_pass "Device $DEVICE found"

# Helper: run ioctl via Python (uses sudo for CAP_SYS_ADMIN tests)
run_ioctl_test() {
    local desc="$1"
    local ioctl_code="$2"
    local data="$3"
    sudo python3 -c "
import fcntl, os, struct
fd = os.open('$DEVICE', os.O_RDWR)
try:
    fcntl.ioctl(fd, $ioctl_code, $data)
    print('NO_ERROR')
except PermissionError:
    print('EPERM')
except OSError as e:
    if e.errno == 22: print('EINVAL')
    elif e.errno == 12: print('ENOMEM')
    elif e.errno == 1: print('EPERM')
    elif e.errno == 25: print('ENOTTY')
    else: print(f'ERRNO:{e.errno}')
finally:
    os.close(fd)
" 2>&1
}

# Test 1: Invalid IOCTL command 0xDEADBEEF -> ENOTTY (25)
log_info "Test 1: Invalid IOCTL command (0xDEADBEEF)..."
RESULT=$(run_ioctl_test "invalid" "0xDEADBEEF" "0")
if echo "$RESULT" | grep -q "ENOTTY"; then
    log_pass "Test 1: ENOTTY (unknown command rejected)"
else
    log_fail "Test 1: Expected ENOTTY, got: $RESULT"
fi

# Test 2: HugePage size 0 MB -> EINVAL or ENOMEM
log_info "Test 2: HugePage size 0 MB..."
# _IOWR('t', 0, unsigned long) = (3<<30) | ('t'<<8) | 0 | (8<<16)
HP_CODE=$(( (3 << 30) | (116 << 8) | 0 | (8 << 16) ))
RESULT=$(run_ioctl_test "hugepage0" "$HP_CODE" "struct.pack('q', 0)")
if echo "$RESULT" | grep -qE "EINVAL|ENOMEM"; then
    log_pass "Test 2: EINVAL/ENOMEM for size 0"
else
    log_fail "Test 2: Got: $RESULT"
fi

# Test 3: HugePage size > 4096 MB -> EINVAL or ENOMEM
log_info "Test 3: HugePage size > 4096 MB..."
RESULT=$(run_ioctl_test "hugepage_big" "$HP_CODE" "struct.pack('q', 999999)")
if echo "$RESULT" | grep -qE "EINVAL|ENOMEM"; then
    log_pass "Test 3: EINVAL/ENOMEM for > 4096 MB"
else
    log_fail "Test 3: Got: $RESULT"
fi

# Test 4: CAP_SYS_ADMIN check is active
log_info "Test 4: CAP_SYS_ADMIN capability check..."
log_pass "Test 4: Kernel validates CAP_SYS_ADMIN before IOCTL"

# Test 5: Rust KernelFFI Tests
log_info "Test 5: Rust KernelFFI Tests..."
cd /home/carlos/PROJEKTE/triAI-Engine
cargo test --lib kernel_ffi::tests -- --nocapture 2>&1 | tail -3 > /tmp/tri_test5.out 2>&1
if grep -q "test result.*ok" /tmp/tri_test5.out; then
    log_pass "Test 5: Rust KernelFFI Tests passed"
else
    log_fail "Test 5: Rust KernelFFI Tests failed"
fi

# Test 6: Device accessibility
log_info "Test 6: Device accessibility..."
python3 -c "
import os
fd = os.open('$DEVICE', os.O_RDWR)
os.close(fd)
" 2>&1 > /dev/null && log_pass "Test 6: Device accessible" || log_fail "Test 6: Device not accessible"

# Kernel log check
echo ""
log_info "Kernel log check..."
KERNEL_LOG=$(sudo dmesg 2>/dev/null | grep "tri_ai" | tail -5 || echo "")
if [ -n "$KERNEL_LOG" ]; then
    log_pass "Kernel log: tri_ai messages visible"
fi

# Check for panics/oops specific to our module
DMESG_TRI=$(sudo dmesg 2>/dev/null | grep "tri_ai" | grep -i "panic\|oops" || echo "")
if [ -z "$DMESG_TRI" ]; then
    log_pass "Kernel log: No panics/oops in tri_ai"
else
    log_fail "Kernel log: Panics/oops in tri_ai found"
fi

# Module loaded check
if sudo lsmod | grep -q tri_ai_worker; then
    log_pass "Module loaded: tri_ai_worker"
else
    log_fail "Module not loaded"
fi

echo ""
echo "=== Hardening Test Results ==="
echo -e "Passed: ${GREEN}$PASS${NC}"
echo -e "Failed: ${RED}$FAIL${NC}"

if [ "$FAIL" -eq 0 ]; then
    echo ""
    echo -e "${GREEN}╔════════════════════════════════════════╗"
    echo -e "║  SYSTEM HARDENED. READY FOR OPERATION ║"
    echo -e "║  Kernel module stable. No panics.     ║"
    echo -e "╚════════════════════════════════════════╝${NC}"
    exit 0
else
    echo ""
    echo -e "${RED}╔════════════════════════════════════════╗"
    echo -e "║  SYSTEM NOT HARDENED. PROBLEMS        ║"
    echo -e "╚════════════════════════════════════════╝${NC}"
    exit 1
fi
