//! Slot Management (decoupled).
//!
//! Dieser Modul kapselt die logische Invariante des "Single Model Slot". Anstatt auf komplexes,
//! plattformspezifisches Locking (`flock`, `UnixStream`) zu setzen, wird hier eine rein zustandsbasierte
//! Abstraktion verwendet. Der Fokus liegt darauf, nur den *Status* (besetzt/frei) zu prüfen, ohne
//! die eigentliche Betriebssystem-Ressourcenverwaltung durchzuführen. Dies macht das Modul portierbar und isoliert.

use std::sync::{Mutex, OnceLock};

/// Bietet einen globalen, statischen Status für die Verfügbarkeit des Slots.
pub struct SlotTracker {
    owner: Mutex<Option<String>>,
}

impl SlotTracker {
    const fn new() -> Self {
        Self {
            owner: Mutex::new(None),
        }
    }

    /// Gibt den statischen, globalen Tracker-Singleton zurück.
    pub fn instance() -> &'static SlotTracker {
        static SLOT_INSTANCE: OnceLock<SlotTracker> = OnceLock::new();
        SLOT_INSTANCE.get_or_init(SlotTracker::new)
    }

    /// Prüft, ob der Slot momentan verfügbar ist (kein anderer Eigentümer registriert).
    pub fn is_available() -> bool {
        // Ein einfaches Lesen des Mutex-Inhalts reicht aus, um den Zustand zu prüfen.
        let owner = Self::instance().owner.lock().unwrap();
        owner.is_none()
    }

    /// Reserviert den Slot für einen bestimmten Besitzer (Owner).
    /// Dies ist ein logischer Vorgang und ändert nichts am Betriebssystem-Status.
    pub fn acquire(owner: &str) -> Result<(), &'static str> {
        let mut owner_lock = Self::instance().owner.lock().unwrap();
        if owner_lock.is_some() {
            return Err("Slot is already occupied");
        }
        *owner_lock = Some(owner.to_string());
        Ok(())
    }

    /// Gibt den Slot frei und setzt den Besitzer auf None zurück.
    pub fn release() {
        let mut owner_lock = Self::instance().owner.lock().unwrap();
        if owner_lock.is_some() {
            *owner_lock = None;
        }
    }

    /// Gibt den aktuellen Besitzer des Slots zurück.
    pub fn current_owner() -> Option<String> {
        let owner_lock = Self::instance().owner.lock().unwrap();
        owner_lock.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Wichtig: Da wir einen globalen Singleton verwenden, müssen Tests die Umgebung sauber aufräumen.
    fn setup() {
        SlotTracker::release(); // Sicherstellen, dass der Slot leer ist
    }

    #[test]
    fn slot_can_be acquired_and_released() {
        setup();
        let owner_name = "test-owner";

        assert!(SlotTracker::is_available());

        // Acquire
        assert!(SlotTracker::acquire(owner_name).is_ok());
        assert_eq!(SlotTracker::current_owner(), Some(owner_name.to_string()));

        // Prüfen, ob es jetzt nicht verfügbar ist
        assert!(!SlotTracker::is_available());

        // Release
        SlotTracker::release();
        assert!(SlotTracker::is_available());
    }

    #[test]
    fn multiple_processes_simulate_concurrency() {
        setup();
        let owner1 = "proc-a";
        let owner2 = "proc-b";

        // Simuliere zwei Prozesse, die gleichzeitig zugreifen wollen (nur der erste gewinnt)
        assert!(SlotTracker::acquire(owner1).is_ok());
        assert!(!SlotTracker::is_available());

        // Zweiter Prozess versucht zu acquiren und scheitert
        let result2 = SlotTracker::acquire(owner2);
        assert!(result2.is_err());
        assert_eq!(result2.unwrap_err(), "Slot is already occupied");
    }
}