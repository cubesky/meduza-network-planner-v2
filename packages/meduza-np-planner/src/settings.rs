//! Local registration settings (`/etc/meduza-np/manager.json`, §11.3).

use std::io;
use std::path::Path;

use meduza_np_protocol::config::NodeBehavior;
use meduza_np_runtime::fsutil;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagerSettings {
    /// `https://manager.example.test` (http:// only accepted with `allowInsecure` for development).
    pub manager_url: String,
    pub node_id: String,
    /// Extra trust anchors (PEM) for the manager certificate, in addition to the system store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    /// Behavior last confirmed by the manager; the published config is authoritative once received.
    #[serde(default)]
    pub behavior: NodeBehavior,
    #[serde(default)]
    pub allow_insecure: bool,
}

impl ManagerSettings {
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        fsutil::read_json(path, 64 * 1024)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        fsutil::write_json_atomic(path, self, 0o600)
    }

    pub fn validate(&self) -> Result<(), String> {
        let url = &self.manager_url;
        if url.starts_with("https://") || (self.allow_insecure && url.starts_with("http://")) {
            if url.trim_end_matches('/').matches('/').count() > 2 {
                return Err("managerUrl must be an origin without a path".into());
            }
            Ok(())
        } else {
            Err("managerUrl must start with https:// (http:// requires allowInsecure, development only)".into())
        }
    }

    pub fn base(&self) -> &str {
        self.manager_url.trim_end_matches('/')
    }
}
