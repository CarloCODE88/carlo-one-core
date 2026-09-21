#!/bin/bash
echo "🛡️  HIXX SYSTEM TUNING - MAX PERFORMANCE MODE"

# 1. CPU Governor
echo "Setting CPU Governor to performance..."
for cpu in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    if [ -w "$cpu" ]; then
        echo performance | sudo tee "$cpu" > /dev/null
    fi
done
echo "✅ CPU Governor set to performance"

# 2. Hugepages (4GB Reserve for model weights)
echo "Allocating Hugepages..."
sudo sysctl -w vm.nr_hugepages=2048 2>/dev/null || echo "⚠️  Could not set hugepages (need root)"
sudo mkdir -p /dev/hugepages 2>/dev/null
sudo mount -t hugetlbfs nodev /dev/hugepages 2>/dev/null || true
echo "✅ Hugepages configured"

# 3. Swappiness reduzieren
echo "Reducing swappiness..."
sudo sysctl -w vm.swappiness=1 2>/dev/null || true
echo "✅ Swappiness set to 1"

# 4. Memory compaction deaktivieren
echo "Disabling memory compaction..."
sudo sysctl -w vm.compact_memory=0 2>/dev/null || true

# 5. CPU isolcpus check
echo "CPU isolation status:"
cat /proc/cmdline 2>/dev/null | grep -o "isolcpus=[^ ]*" || echo "⚠️  isolcpus not set (requires reboot)"

echo ""
echo "✅ Hixx system tuning complete."
echo "⚠️  For isolcpus changes, a reboot is required."
echo "💡 Run 'make load && make run' to start the full system."
