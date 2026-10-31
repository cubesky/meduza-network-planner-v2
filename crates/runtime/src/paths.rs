//! Centralized node paths (§11.3). Every component resolves locations through [`NodePaths`] so the
//! OpenWrt overlay / container volume layout is configured in one place.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePaths {
    /// `/etc/meduza-np` — registration, manager settings, node identity, trust roots.
    pub etc: PathBuf,
    /// `/var/lib/meduza-np` (OpenWrt: `/etc/meduza-np-state`) — last good config, journals, ownership.
    pub state: PathBuf,
    /// `/run/meduza-np` — sockets, generations, runtime observations.
    pub run: PathBuf,
    /// `/usr/share/meduza-np/packages` — installed manifests, schemas, UI descriptors, templates.
    pub packages: PathBuf,
    /// `/usr/libexec/meduza-np` — plugins and hooks.
    pub libexec: PathBuf,
}

impl NodePaths {
    /// Defaults (OpenWrt-specific state/run dirs when `/etc/openwrt_release` exists), overridable with
    /// `MEDUZA_NP_{ETC,STATE,RUN,PACKAGES,LIBEXEC}_DIR` (container entrypoint, tests).
    pub fn from_env() -> Self {
        let get = |var: &str, default: &str| std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default));
        // OpenWrt: persistent state lives on the overlay, runtime files under /var/run (tmpfs). Every component (planner,
        // CLI, rpcd plugin, updater) must agree without each init script exporting the same variables.
        let openwrt = Path::new("/etc/openwrt_release").exists();
        Self {
            etc: get("MEDUZA_NP_ETC_DIR", "/etc/meduza-np"),
            state: get("MEDUZA_NP_STATE_DIR", if openwrt { "/etc/meduza-np-state" } else { "/var/lib/meduza-np" }),
            run: get("MEDUZA_NP_RUN_DIR", if openwrt { "/var/run/meduza-np" } else { "/run/meduza-np" }),
            packages: get("MEDUZA_NP_PACKAGES_DIR", "/usr/share/meduza-np/packages"),
            libexec: get("MEDUZA_NP_LIBEXEC_DIR", "/usr/libexec/meduza-np"),
        }
    }

    /// All paths under one root (tests, dev runs).
    pub fn under(root: &Path) -> Self {
        Self {
            etc: root.join("etc"),
            state: root.join("state"),
            run: root.join("run"),
            packages: root.join("packages"),
            libexec: root.join("libexec"),
        }
    }

    pub fn identity_dir(&self) -> PathBuf {
        self.etc.join("identity")
    }
    pub fn manager_settings(&self) -> PathBuf {
        self.etc.join("manager.json")
    }
    pub fn last_good_config(&self) -> PathBuf {
        self.state.join("config").join("last-good.json")
    }
    pub fn last_good_secrets(&self) -> PathBuf {
        self.state.join("config").join("secrets.json")
    }
    pub fn pending_journal(&self) -> PathBuf {
        self.state.join("journal").join("pending.json")
    }
    pub fn ownership(&self) -> PathBuf {
        self.state.join("ownership.json")
    }
    pub fn sync_state(&self) -> PathBuf {
        self.state.join("sync.json")
    }
    pub fn generations(&self) -> PathBuf {
        self.run.join("generations")
    }
    pub fn current_generation_link(&self) -> PathBuf {
        self.run.join("current")
    }
    pub fn planner_socket(&self) -> PathBuf {
        self.run.join("planner.sock")
    }
    pub fn access_socket(&self) -> PathBuf {
        self.run.join("access.sock")
    }
    pub fn updater_socket(&self) -> PathBuf {
        self.run.join("updater.sock")
    }
    /// Components the current configuration needs (JSON list of short names): written by planner, installed by
    /// the updater on top of its own selection (D20).
    pub fn wanted_components(&self) -> PathBuf {
        self.run.join("wanted-components.json")
    }
    pub fn providers_state(&self) -> PathBuf {
        self.state.join("providers")
    }
    pub fn maintenance(&self) -> PathBuf {
        self.state.join("maintenance.json")
    }
    pub fn logs(&self) -> PathBuf {
        self.run.join("logs")
    }
}
