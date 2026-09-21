use libc::{c_int, c_ulong, c_void};
use std::fs::File;
use std::io;
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::ptr::NonNull;

const DEVICE_PATH: &str = "/dev/tri_ai_worker";
const RING_BUFFER_SIZE: usize = 4 * 1024 * 1024;
const HIXX_IOC_MAGIC: u32 = 0x48;

const IOC_NONE: u32 = 0;
const IOC_READ: u32 = 2;
const fn ioctl_request(direction: u32, number: u32, size: u32) -> c_ulong {
    ((direction << 30) | (size << 16) | (HIXX_IOC_MAGIC << 8) | number) as c_ulong
}

const IOCTL_INIT: c_ulong = ioctl_request(IOC_NONE, 0, 0);
const IOCTL_SUBMIT: c_ulong = ioctl_request(IOC_NONE, 1, 0);
const IOCTL_GET_STATUS: c_ulong = ioctl_request(IOC_READ, 2, std::mem::size_of::<c_int>() as u32);

pub struct HixxIPC {
    file: File,
    buffer_ptr: NonNull<c_void>,
}

impl HixxIPC {
    pub fn new() -> io::Result<Self> {
        let file = File::options().read(true).write(true).open(DEVICE_PATH)?;
        let fd = file.as_raw_fd();

        if unsafe { libc::ioctl(fd, IOCTL_INIT) } == -1 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("INIT ioctl failed: {error}"),
            ));
        }

        let size = NonZeroUsize::new(RING_BUFFER_SIZE).expect("ring buffer size is non-zero");
        let buffer_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size.get(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if buffer_ptr == libc::MAP_FAILED {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("ring-buffer mmap failed: {error}"),
            ));
        }

        Ok(Self {
            file,
            buffer_ptr: NonNull::new(buffer_ptr).expect("mmap never returns null"),
        })
    }

    pub fn submit_task(&self) -> io::Result<()> {
        if unsafe { libc::ioctl(self.file.as_raw_fd(), IOCTL_SUBMIT) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn get_status(&self) -> io::Result<i32> {
        let mut status = 0;
        if unsafe { libc::ioctl(self.file.as_raw_fd(), IOCTL_GET_STATUS, &mut status) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(status)
    }
}

impl Drop for HixxIPC {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.buffer_ptr.as_ptr(), RING_BUFFER_SIZE);
        }
    }
}

unsafe impl Send for HixxIPC {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_encodings_match_linux_layout() {
        assert_eq!(IOCTL_INIT, 0x0000_4800);
        assert_eq!(IOCTL_SUBMIT, 0x0000_4801);
        assert_eq!(IOCTL_GET_STATUS, 0x8004_4802);
    }
}
