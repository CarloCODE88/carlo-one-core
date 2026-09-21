use serde::{Deserialize, Serialize};
use std::{fs, io, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub vram_total_mb: u64,
    pub vram_used_mb: u64,
    pub vram_free_mb: u64,
    pub ram_total_mb: u64,
    pub ram_available_mb: u64,
    pub ssd_total_mb: u64,
    pub ssd_free_mb: u64,
}

pub fn read() -> io::Result<Snapshot> {
    // NVIDIA ist optional: auf CPU-only-Systemen oder in restriktiven
    // Sandboxes darf ein fehlendes/temporär nicht erreichbares `nvidia-smi`
    // nicht den ganzen Runner blockieren. Null VRAM zwingt den Planner in
    // eine sichere RAM-/CPU-Lane; RAM- und SSD-Messung bleiben hingegen
    // harte Voraussetzungen.
    let (vram_total_mb, vram_used_mb, vram_free_mb) = read_gpu().unwrap_or((0, 0, 0));
    let (ram_total_mb, ram_available_mb) = read_ram()?;
    let (ssd_total_mb, ssd_free_mb) = read_ssd(".")?;
    Ok(Snapshot {
        vram_total_mb,
        vram_used_mb,
        vram_free_mb,
        ram_total_mb,
        ram_available_mb,
        ssd_total_mb,
        ssd_free_mb,
    })
}

fn read_gpu() -> io::Result<(u64, u64, u64)> {
    let out = Command::new("nvidia-smi")
        .args([
            "--query-gpu=memory.total,memory.used,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other("nvidia-smi failed"));
    }
    let output = String::from_utf8_lossy(&out.stdout);
    let line = output.lines().next().unwrap_or("");
    let values: Vec<u64> = line
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    if values.len() != 3 {
        return Err(io::Error::other("unexpected nvidia-smi output"));
    }
    Ok((values[0], values[1], values[2]))
}

fn read_ram() -> io::Result<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let mut total = 0;
    let mut available = 0;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("MemTotal:") => {
                total = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0) / 1024
            }
            Some("MemAvailable:") => {
                available = parts.next().unwrap_or("0").parse::<u64>().unwrap_or(0) / 1024
            }
            _ => {}
        }
    }
    if total == 0 {
        return Err(io::Error::other("MemTotal missing"));
    }
    Ok((total, available))
}

fn read_ssd(path: &str) -> io::Result<(u64, u64)> {
    let c_path = std::ffi::CString::new(path).map_err(io::Error::other)?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let block = stat.f_frsize;
    Ok((
        (stat.f_blocks * block) / 1_048_576,
        (stat.f_bavail * block) / 1_048_576,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_workspace_filesystem() {
        let (total, free) = read_ssd(".").unwrap();
        assert!(total > 0);
        assert!(free <= total);
    }

    #[test]
    fn snapshot_still_has_ram_and_ssd_when_gpu_query_fails() {
        let snapshot = read().unwrap();
        assert!(snapshot.ram_total_mb > 0);
        assert!(snapshot.ssd_total_mb > 0);
    }

    // Exercises the resource-fallback chain in `planner::plan` down
    // to its last safe lane: GPU doesn't fit, the RAM+GPU-balanced lane
    // doesn't fit either (full estimated RAM need exceeds usable RAM), but
    // the model file still fits on the SSD and the bare KV-cache overhead
    // alone fits in the remaining RAM, so CpuSafe is the only safe lane left
    // before Reject. GpuFast/RamBalanced/Reject are already covered in
    // main.rs; CpuSafe was not.
    #[test]
    fn falls_back_to_cpu_safe_when_only_kv_overhead_fits_in_ram() {
        let profile = crate::planner::ModelProfile {
            model: "m.gguf".into(),
            file_size_mb: 5000,
            estimated_gpu_mb: 6000,
            estimated_ram_mb: 7000,
            layers: 32,
            context_tokens: 8192,
            requested_context_tokens: 4096,
        };
        let resources = crate::planner::Resources {
            vram_free_mb: 1000,
            ram_free_mb: 2000,
            ssd_free_mb: 20000,
            vram_reserve_mb: 500,
            ram_reserve_mb: 500,
            ssd_reserve_mb: 1000,
        };
        let result = crate::planner::plan(&profile, &resources);
        assert_eq!(result.lane, crate::planner::Lane::CpuSafe);
        assert!(result.safe);
        assert_eq!(result.gpu_layers, 0);
    }
}
