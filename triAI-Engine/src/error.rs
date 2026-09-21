//! Error types for triAI engine.

use std::fmt;

#[derive(Debug)]
pub enum TriAIError {
    EvidenceWrite(String),
    Io(std::io::Error),
    Serialization(serde_json::Error),
    Other(String),
}

impl fmt::Display for TriAIError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TriAIError::EvidenceWrite(msg) => write!(f, "Evidence write error: {}", msg),
            TriAIError::Io(err) => write!(f, "IO error: {}", err),
            TriAIError::Serialization(err) => write!(f, "Serialization error: {}", err),
            TriAIError::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for TriAIError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TriAIError::Io(err) => Some(err),
            TriAIError::Serialization(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TriAIError {
    fn from(err: std::io::Error) -> Self {
        TriAIError::Io(err)
    }
}

impl From<serde_json::Error> for TriAIError {
    fn from(err: serde_json::Error) -> Self {
        TriAIError::Serialization(err)
    }
}

pub type Result<T> = std::result::Result<T, TriAIError>;
