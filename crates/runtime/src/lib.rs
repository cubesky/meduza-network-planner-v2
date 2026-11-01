//! Runtime primitives shared by planner, updater, CLI and dashboard. Deliberately small: service templates,
//! web assets and downloaders are not linked through here (§4).

pub mod backoff;
pub mod fsutil;
pub mod jitter;
pub mod journal;
pub mod paths;
pub mod update;
