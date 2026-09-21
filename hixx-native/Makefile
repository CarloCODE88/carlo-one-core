.PHONY: all kernel rust clean tune load status

HIXX_DEVICE_GROUP ?= users

all: kernel rust

kernel:
	@echo "🔨 Building Kernel Module..."
	$(MAKE) -C kernel

rust:
	@echo "🦀 Building Rust Daemon..."
	cargo build --release

tune:
	@echo "⚡ Running System Tuning..."
	sudo ./scripts/hixx_tune_system.sh

load: kernel
	@echo "📥 Loading Kernel Module..."
	sudo rmmod tri_ai_worker 2>/dev/null || true
	sudo insmod kernel/tri_ai_worker.ko
	sudo chgrp $(HIXX_DEVICE_GROUP) /dev/tri_ai_worker
	sudo chmod 660 /dev/tri_ai_worker
	@lsmod | grep tri_ai_worker && echo "✅ Module loaded" || echo "⚠️  Module load failed"

status:
	@echo "=== Hixx-Native Status ==="
	@lsmod | grep tri_ai_worker && echo "✅ Kernel module loaded" || echo "❌ Module not loaded"
	@ls -l /dev/tri_ai_worker 2>/dev/null && echo "✅ Device node exists" || echo "❌ Device not found"
	@cargo build --release 2>&1 | tail -1 || echo "❌ Rust build failed"

clean:
	$(MAKE) -C kernel clean
	cargo clean

run: load rust
	@echo "🚀 Starting Hixx-Server..."
	./target/release/hixx-server
