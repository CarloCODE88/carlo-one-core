//! Process-local half of the one-model-slot invariant.
//!
//! The worker supervisor additionally owns a filesystem `flock` so separate
//! engine processes cannot race. This guard covers concurrent Rust callers in
//! the same process and releases through RAII on every error/shutdown path.

use std::{
    io,
    sync::{Mutex, OnceLock},
};

pub struct AtomicSlot {
    owner: Mutex<Option<String>>,
}

pub struct SlotLease {
    slot: &'static AtomicSlot,
    owner: String,
}

impl AtomicSlot {
    pub const fn new() -> Self {
        Self {
            owner: Mutex::new(None),
        }
    }
    pub fn acquire(&'static self, owner: &str) -> io::Result<SlotLease> {
        let mut current = self
            .owner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "single-model slot is busy",
            ));
        }
        *current = Some(owner.to_owned());
        Ok(SlotLease {
            slot: self,
            owner: owner.to_owned(),
        })
    }
    pub fn owner(&self) -> Option<String> {
        self.owner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        let mut current = self
            .slot
            .owner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.as_deref() == Some(self.owner.as_str()) {
            *current = None;
        }
    }
}

static GLOBAL_SLOT: OnceLock<AtomicSlot> = OnceLock::new();
pub fn global() -> &'static AtomicSlot {
    GLOBAL_SLOT.get_or_init(AtomicSlot::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn second_lease_is_rejected_until_first_is_dropped() {
        let slot = Box::leak(Box::new(AtomicSlot::new()));
        let first = slot.acquire("ingried").unwrap();
        assert!(slot.acquire("dolphin3").is_err());
        drop(first);
        assert!(slot.acquire("dolphin3").is_ok());
    }
}
