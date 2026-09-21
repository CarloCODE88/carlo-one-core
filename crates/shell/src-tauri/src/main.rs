#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use tauri::Manager;

#[tauri::command]
async fn initialize_engine() -> Result<String, String> {
    // TODO: Phase 1 – EngineAdapter aufrufen
    Ok("Engine initialized (placeholder)".to_string())
}

#[tauri::command]
async fn submit_job(prompt: String, task_type: String) -> Result<String, String> {
    // TODO: Phase 1 – JobOrchestrator aufrufen
    Ok("job-id-placeholder".to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            initialize_engine,
            submit_job,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
