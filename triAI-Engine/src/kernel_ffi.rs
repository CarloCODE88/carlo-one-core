#![allow(clippy::all)]
//! Rust-Festung: FFI-Bridge zum Kernel-Modul.
//! Keine unwrap, kein panic. Nur kalte Error-Propagation.

use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::{Arc, Mutex};

use libc::{c_int, c_long, c_void, ioctl, loff_t, size_t};

const DEVICE_PATH: &str = "/dev/tri_ai_worker";
const TRI_IOCTL_MAGIC: u8 = b't';

const fn iowr(nr: u8, size: usize) -> c_long {
    let dir: c_long = 2;
    let type_: c_long = TRI_IOCTL_MAGIC as c_long;
    let nr: c_long = nr as c_long;
    let size: c_long = size as c_long;
    (dir << 30) | (type_ << 8) | nr | (size << 16)
}

const TRI_IOCTL_ALLOC_HUGEPAGE: c_long = iowr(0, std::mem::size_of::<c_long>());
const TRI_IOCTL_PIN_CPU: c_long = iowr(1, std::mem::size_of::<c_int>());
const TRI_IOCTL_PREFETCH_DISK: c_long = iowr(2, std::mem::size_of::<PrefetchReq>());
const TRI_IOCTL_EVICT: c_long = iowr(3, std::mem::size_of::<c_long>());
const TRI_IOCTL_GET_STATS: c_long = iowr(4, std::mem::size_of::<TriStats>());

#[repr(C)]
pub struct PrefetchReq {
    pub fd: c_int,
    pub offset: loff_t,
    pub len: size_t,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct TriStats {
    pub vram_pages: c_long,
    pub ram_pages: c_long,
    pub disk_pages: c_long,
    pub evictions: c_long,
    pub prefetches: c_long,
    pub overall_pressure: c_int,
}

#[repr(C)]
#[derive(Default, Clone, Copy, PartialEq)]
pub enum Tier {
    #[default]
    Vram = 0,
    Ram = 1,
    Disk = 2,
}

#[derive(Debug)]
pub enum KernelError {
    DeviceNotFound(String),
    IoctlFailed(io::Error),
    NoCapacity(String),
    PermissionDenied,
    InvalidSize(String),
    InvalidCpuId(String),
    Internal(String),
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KernelError::DeviceNotFound(p) => write!(f, "Device not found: {}", p),
            KernelError::IoctlFailed(e) => write!(f, "Ioctl failed: {}", e),
            KernelError::NoCapacity(m) => write!(f, "No capacity: {}", m),
            KernelError::PermissionDenied => write!(f, "Permission denied"),
            KernelError::InvalidSize(m) => write!(f, "Invalid size: {}", m),
            KernelError::InvalidCpuId(m) => write!(f, "Invalid CPU: {}", m),
            KernelError::Internal(m) => write!(f, "Internal error: {}", m),
        }
    }
}

impl std::error::Error for KernelError {}

pub type KernelResult<T> = Result<T, KernelError>;

pub struct KernelWorker {
    fd: RawFd,
    stats: Arc<Mutex<TriStats>>,
}

impl KernelWorker {
    pub fn new() -> KernelResult<Self> {
        let path = CString::new(DEVICE_PATH).map_err(|e| KernelError::Internal(e.to_string()))?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == io::ErrorKind::NotFound {
                Err(KernelError::DeviceNotFound(DEVICE_PATH.to_string()))
            } else if err.kind() == io::ErrorKind::PermissionDenied {
                Err(KernelError::PermissionDenied)
            } else {
                Err(KernelError::IoctlFailed(err))
            };
        }

        let mut stats = TriStats::default();
        let ret = unsafe {
            ioctl(fd, TRI_IOCTL_GET_STATS as u64, &mut stats as *mut TriStats as *mut c_void)
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            let _ = unsafe { libc::close(fd) };
            return Err(KernelError::IoctlFailed(err));
        }

        eprintln!("[tri_ai_rust] Kernel Worker verbunden. vram={} ram={} pressure={}",
                   stats.vram_pages, stats.ram_pages, stats.overall_pressure);

        Ok(Self { fd, stats: Arc::new(Mutex::new(stats)) })
    }

    fn validate_hugepage_size(size_mb: u64) -> KernelResult<()> {
        if size_mb == 0 {
            return Err(KernelError::InvalidSize("Size must be > 0".to_string()));
        }
        if size_mb > 4096 {
            return Err(KernelError::InvalidSize(format!(
                "Size {} MB exceeds hard limit 4096 MB", size_mb
            )));
        }
        Ok(())
    }

    fn validate_cpu_id(cpu_id: c_int) -> KernelResult<()> {
        let max_cpu = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
        if cpu_id < 0 || cpu_id as c_long >= max_cpu {
            return Err(KernelError::InvalidCpuId(format!(
                "CPU {} invalid (0-{} available)", cpu_id, max_cpu - 1
            )));
        }
        Ok(())
    }

    pub fn allocate_hugepage(&self, size_mb: u64) -> KernelResult<()> {
        Self::validate_hugepage_size(size_mb)?;
        let mut size = size_mb as c_long;
        let ret = unsafe {
            ioctl(self.fd as c_int, TRI_IOCTL_ALLOC_HUGEPAGE as u64, &mut size as *mut c_long as *mut c_void)
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            return if err.raw_os_error() == Some(libc::ENOMEM) {
                Err(KernelError::NoCapacity(format!("Cannot allocate {} MB hugepage", size_mb)))
            } else {
                Err(KernelError::IoctlFailed(err))
            };
        }
        Ok(())
    }

    pub fn pin_current_thread_to_cpu(&self, cpu_id: c_int) -> KernelResult<()> {
        Self::validate_cpu_id(cpu_id)?;
        let mut id = cpu_id;
        let ret = unsafe {
            ioctl(self.fd as c_int, TRI_IOCTL_PIN_CPU as u64, &mut id as *mut c_int as *mut c_void)
        };
        if ret < 0 {
            return Err(KernelError::IoctlFailed(io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn request_disk_prefetch(&self, fd: c_int, offset: u64, len: size_t) -> KernelResult<()> {
        if len == 0 || len > 1usize << 40 {
            return Err(KernelError::InvalidSize(format!(
                "Prefetch length {} invalid (0 < len <= 1TiB)", len
            )));
        }
        let mut req = PrefetchReq { fd, offset: offset as loff_t, len };
        let ret = unsafe {
            ioctl(self.fd as c_int, TRI_IOCTL_PREFETCH_DISK as u64, &mut req as *mut PrefetchReq as *mut c_void)
        };
        if ret < 0 {
            return Err(KernelError::IoctlFailed(io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn evict_cold_pages(&self, min_priority: u64) -> KernelResult<()> {
        let mut priority = min_priority as c_long;
        let ret = unsafe {
            ioctl(self.fd as c_int, TRI_IOCTL_EVICT as u64, &mut priority as *mut c_long as *mut c_void)
        };
        if ret < 0 {
            return Err(KernelError::IoctlFailed(io::Error::last_os_error()));
        }
        Ok(())
    }

    pub fn get_stats(&self) -> KernelResult<TriStats> {
        let mut stats = TriStats::default();
        let ret = unsafe {
            ioctl(self.fd as c_int, TRI_IOCTL_GET_STATS as u64, &mut stats as *mut TriStats as *mut c_void)
        };
        if ret < 0 {
            return Err(KernelError::IoctlFailed(io::Error::last_os_error()));
        }
        Ok(stats)
    }

    pub fn raw_fd(&self) -> RawFd { self.fd }
    pub fn is_device_ready(&self) -> bool { self.get_stats().is_ok() }
}

impl Drop for KernelWorker {
    fn drop(&mut self) {
        let _ = unsafe { libc::close(self.fd) };
        eprintln!("[tri_ai_rust] Kernel Worker geschlossen (fd={})", self.fd);
    }
}

pub fn check_device_ready() -> KernelResult<()> {
    if !Path::new(DEVICE_PATH).exists() {
        return Err(KernelError::DeviceNotFound(DEVICE_PATH.to_string()));
    }
    let worker = KernelWorker::new()?;
    if !worker.is_device_ready() {
        return Err(KernelError::Internal("Device not responding".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_hugepage_size() {
        assert!(KernelWorker::validate_hugepage_size(1).is_ok());
        assert!(KernelWorker::validate_hugepage_size(4096).is_ok());
        assert!(KernelWorker::validate_hugepage_size(0).is_err());
        assert!(KernelWorker::validate_hugepage_size(4097).is_err());
    }

    #[test]
    fn test_stats_struct_size() {
        assert_eq!(std::mem::size_of::<TriStats>(), 6 * 8);
    }

    #[test]
    fn test_prefetch_req_size() {
        assert_eq!(std::mem::size_of::<PrefetchReq>(), 8 + 8 + 8);
    }
}