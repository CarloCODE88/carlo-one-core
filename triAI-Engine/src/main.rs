//! Haupt-Anwendungspunkt für den triAI-Engine.
//!
//! Dieser Modul ist jetzt die oberste Schicht, die nur noch die öffentlichen Schnittstellen
//! (APIs) der de-coupled Core-Module aufruft. Er enthält KEINE interne Logik zu
//! Model-Handling oder Worker-Management, sondern dient lediglich dem Orchestrieren des
//! Startprozesses und des API-Gateways.

use std::{sync::Arc, time::Duration};
use tri_ai_engine::{
    api::Response, // Importiere den neuen API-Vertrag
    config::Config,
    core::{model_catalog::ModelCatalog, tool_registry::ToolRegistry},
    engine_state::{Engine, EngineState}, // Nutze die abstrakte Engine
};

// Simuliert das Laden des globalen Model Catalog anstelle von direktem FS/API-Zugriff.
fn initialize_model_system(config: &Config) -> ModelCatalog {
    let mut catalog = ModelCatalog::new();
    println!("INFO: Initialisiere ModelCatalog mit konfigurierten Pfaden...");

    // Hier würde die Logik einsetzen, um alle Manifeste im Konfigurationsordner zu finden
    // und sie sequenziell durch `ModelCatalog::add_*` zu verarbeiten.
    // Da dies eine Simulation ist, fügen wir nur einen Platzhalter hinzu.
    catalog.add_direct_gguf(
        config.paths.model_dir.clone(),
        crate::model_catalog::CatalogMetadata { // Verwendung des dekouplierten Typs
            digest: "mocked-manifest-digest".into(),
            size_bytes: Some(1024),
            family: Some("qwen2".into()),
            family_verified: true,
            format: Some("GGUF".into()),
            capabilities: vec!["Chat".into()],
            context_tokens: Some(8192),
            ..Default::default()
        },
    ).expect("Mock Catalog Add Failure");

    println!("INFO: ModelCatalog erfolgreich mit Platzhalterdaten initialisiert.");
    catalog
}


fn main_run_engine_startup(config: &Config) -> Result<Arc<Engine>, Box<dyn std::error::Error>> {
    // 1. Initialisiere die statischen Komponenten (API-Verträge, Tools etc.)
    let tool_registry = ToolRegistry::new(); // Nur Definitions-Lookup

    // 2. Initialisiere den Model Katalog und die Modelle
    let model_catalog = initialize_model_system(config);

    // 3. Starte das Engine State Machine, das nun auf dem Katalogen basiert.
    let mut engine = Engine::new(&config.paths.stage_dir)?; // Nutzt das de-coupled EngineState
    println!("INFO: Engine State Machine initialisiert.");

    // Hier würde die Logik aus Supervisor::miniModelSupervisor::ensure_ready() verwendet,
    // aber anstelle direkter I/O-Aufrufe wird nun auf `engine.start_model_ready()` vertraut.
    // Da wir keine Worker mehr starten können, simulieren wir den Start in Idle und Ready.
    if engine.state() == EngineState::Idle {
        println!("WARN: System startet im Idle State (kein aktiver Worker gestartet).");
    }


    Ok(Arc::new(engine))
}

// Mocking die Hauptfunktionalität für das Beispiel
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn setup_mock_config() -> Config {
        Config {
            paths: Paths {
                event_log: PathBuf::from("./logs"),
                stage_dir: PathBuf::from("./stage/temp"),
                model_dir: PathBuf::from("./models/test.gguf"), // Mocked model source directory
            },
            server: ServerConfig { listen_addr: "127.0.0.1:8080".into() },
        }
    }

    #[test]
    fn full_startup_flow_is_modularized() -> Result<(), Box<dyn std::error::Error>> {
        let config = setup_mock_config();
        // Mocking der gesamte Startup-Prozess, um zu zeigen, dass alle Teile kommunizieren können.
        let engine_arc = main_run_engine_startup(&config)?;

        println!("SUCCESS: Alle Kernmodule wurden erfolgreich miteinander verbunden.");

        Ok(())
    }
}