//! Versioned data transfer objects shared by planner, plugins, CLI, dashboard and updater.
//!
//! The JSON shapes here are the wire contract described in AGENT.md §5, §7, §8, §12 and §13.4.
//! The same shapes are published as JSON Schema under `schemas/` for the TypeScript manager.

pub mod access;
pub mod api;
pub mod bgp;
pub mod config;
pub mod digest;
pub mod events;
pub mod group;
pub mod ids;
pub mod local;
pub mod manifest;
pub mod plan;
pub mod report;
pub mod revision;
pub mod schema;
pub mod ui;

/// Version of the planner ↔ plugin JSON protocol (AGENT.md §5.1).
pub const PLUGIN_PROTOCOL_VERSION: u32 = 1;
/// Version of the planner API that package manifests declare compatibility with (`plannerApi`).
pub const PLANNER_API_VERSION: u32 = 1;
/// Version of the node configuration schema (§8).
pub const CONFIG_SCHEMA_VERSION: u32 = 1;
/// Version of the node report (§12.1).
pub const REPORT_VERSION: u32 = 1;
/// Version of the push event envelope (§7.3).
pub const EVENT_VERSION: u32 = 1;

/// Error raised when a DTO violates an invariant that serde cannot express.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[error("{path}: {message}")]
pub struct ValidationError {
    /// JSON-pointer-like location of the offending value.
    pub path: String,
    pub message: String,
}

impl ValidationError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { path: path.into(), message: message.into() }
    }
}

/// Collects validation errors instead of stopping at the first one, so UIs can show all of them.
#[derive(Debug, Default, Clone)]
pub struct Validator {
    errors: Vec<ValidationError>,
}

impl Validator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn error(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.errors.push(ValidationError::new(path, message));
    }

    pub fn check(&mut self, ok: bool, path: impl Into<String>, message: impl Into<String>) {
        if !ok {
            self.error(path, message);
        }
    }

    pub fn extend(&mut self, other: Validator) {
        self.errors.extend(other.errors);
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn errors(&self) -> &[ValidationError] {
        &self.errors
    }

    pub fn finish(self) -> Result<(), Vec<ValidationError>> {
        if self.errors.is_empty() {
            Ok(())
        } else {
            Err(self.errors)
        }
    }
}
