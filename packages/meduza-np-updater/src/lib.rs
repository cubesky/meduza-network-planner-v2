//! Meduza NP updater (AGENT.md §13, §14.3): independent of planner, checks signed package-group metadata,
//! resolves the node's subset, stages verified packages and switches them under a planner maintenance lock with
//! health verification and a one-generation rollback.

pub mod config;
pub mod engine;
pub mod image;
pub mod meta;
pub mod openwrt;
pub mod planner;
pub mod resolve;
pub mod sig;
pub mod store;

#[cfg(unix)]
pub mod daemon;
