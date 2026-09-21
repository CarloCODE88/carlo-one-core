//! Module Definition for the ToolRegistry module
// Export all necessary components used by external parts of the engine.
pub use super::tool_registry::ToolRegistry;
pub use super::tool_registry::ToolSpec;

// Manchmal ist es notwendig, globale Konstanzen oder Helper-Funktionen zu exportieren,
// die vom ToolRegistry abhängen (z.B. ein is_allowed_tool_name).
// Hier Platzhalter für solche globalen Hilfsfunktionen eintragen:

pub fn get_all_available_tools() -> Vec<&'static ToolSpec> {
    // Mock implementation for export simplicity
    todo!("Implementierung der Werkzeugliste aus dem Registry");
}