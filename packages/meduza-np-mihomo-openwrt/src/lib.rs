//! mihomo-openwrt: Mihomo through OpenClash (AGENT.md §9.8).
//!
//! - OpenClash owns the Mihomo process, its transparent-proxy rules and utun. This package never starts a
//!   second Mihomo and never installs TPROXY rules (it conflicts with `mihomo` and `mihomo-pkg`, §3.2).
//! - The adapter downloads the selected subscription into a *managed* configuration file
//!   `/etc/openclash/config/<configName>.yaml`, strips the keys OpenClash rewrites from its own settings
//!   (ports, controller, DNS, TUN) and shows them as overridden, selects that file in OpenClash if asked and
//!   triggers a controlled restart only when something changed.
//! - Before the first change the original values of every touched UCI option are recorded
//!   (`<state>/mihomo-openwrt/takeover.json`); disabling the instance restores them and removes only the files
//!   the adapter created. User subscriptions and other OpenClash configs are never touched.
//! - Only OpenClash versions in the tested range are driven; anything else is reported as incompatible.

pub mod openclash;
pub mod runner;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::digest::sha256_hex;
use meduza_np_protocol::ids::StableId;
use meduza_np_protocol::plan::{GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SERVICE: &str = "mihomo-openwrt";
pub const INSTANCE: &str = "mihomo-openwrt/main";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "Mihomo via OpenClash")]
pub struct OpenClashConfig {
    /// Disabling hands OpenClash back: recorded options are restored and managed files removed.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// File name (without `.yaml`) of the managed configuration under /etc/openclash/config/.
    #[serde(default = "default_name")]
    pub config_name: String,
    /// Make the managed configuration OpenClash's active `config_path`.
    #[serde(default = "default_true")]
    pub select: bool,
    /// OpenClash running mode (`en_mode`); unset keeps the user's current choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<OpenClashMode>,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_subscription: Option<String>,
    /// Restart OpenClash automatically when the managed config or options change (otherwise use the action).
    #[serde(default = "default_true")]
    pub restart_on_change: bool,
    /// Probe the DNS listener OpenClash provides (`dns_port`) so a dnsmasq front end can use it as backend.
    #[serde(default)]
    pub dns_integration: bool,
    /// Proxy used to fetch subscriptions (must not depend on OpenClash itself being up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_proxy: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_name() -> String {
    "meduza-np".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OpenClashMode {
    RedirHost,
    FakeIp,
    RedirHostTun,
    FakeIpTun,
    RedirHostMix,
    FakeIpMix,
}

impl OpenClashMode {
    pub fn as_uci(self) -> &'static str {
        match self {
            Self::RedirHost => "redir-host",
            Self::FakeIp => "fake-ip",
            Self::RedirHostTun => "redir-host-tun",
            Self::FakeIpTun => "fake-ip-tun",
            Self::RedirHostMix => "redir-host-mix",
            Self::FakeIpMix => "fake-ip-mix",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: StableId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Subscription URL secret (URLs usually embed a token).
    pub url_ref: String,
    #[serde(default = "default_refresh")]
    pub refresh_hours: u32,
    /// Refresh interval in seconds; replaces `refreshHours` (the manager sets it from the global Mihomo settings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 60))]
    pub refresh_seconds: Option<u32>,
}

fn default_refresh() -> u32 {
    12
}

impl Subscription {
    /// Seconds between automatic refreshes.
    pub fn refresh_secs(&self) -> u64 {
        self.refresh_seconds.map(u64::from).unwrap_or(u64::from(self.refresh_hours) * 3600)
    }
}

/// File the `refresh-subscription` action creates; the wrapper re-downloads every subscription when it sees it.
pub fn refresh_trigger(run_dir: &str) -> String {
    format!("{}/mihomo-openwrt.refresh", run_dir.trim_end_matches('/'))
}

pub fn load(req: &PluginRequest) -> Result<Option<OpenClashConfig>, PluginError> {
    req.service::<OpenClashConfig>(SERVICE)
}

pub fn managed_path(cfg: &OpenClashConfig) -> String {
    format!("{}/{}.yaml", openclash::CONFIG_DIR, cfg.config_name)
}

pub fn takeover_path(state_dir: &str) -> std::path::PathBuf {
    Path::new(state_dir).join("mihomo-openwrt").join("takeover.json")
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 48
        && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
        && !n.starts_with('-')
}

/// UCI options the adapter wants set (all within [`openclash::MANAGED_OPTIONS`]).
pub fn desired_options(cfg: &OpenClashConfig) -> BTreeMap<String, String> {
    let mut m = BTreeMap::from([("enable".to_string(), "1".to_string())]);
    if cfg.select {
        m.insert("config_path".into(), managed_path(cfg));
    }
    if let Some(mode) = cfg.mode {
        m.insert("en_mode".into(), mode.as_uci().into());
    }
    m
}

pub fn validate(cfg: &OpenClashConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if !cfg.enabled {
        return Vec::new();
    }
    v.check(valid_name(&cfg.config_name), "/configName", "use lowercase letters, digits, '-' or '_' (max 48)");
    let mut ids = BTreeSet::new();
    for (i, s) in cfg.subscriptions.iter().enumerate() {
        v.check(ids.insert(s.id.clone()), format!("/subscriptions/{i}/id"), "duplicate id");
        v.check(req.config.secrets.contains_key(&s.url_ref), format!("/subscriptions/{i}/urlRef"), "subscription URL secret not delivered");
        match s.refresh_seconds {
            Some(secs) => v.check(secs >= 60, format!("/subscriptions/{i}/refreshSeconds"), "at least 60 seconds"),
            None => v.check(s.refresh_hours >= 1, format!("/subscriptions/{i}/refreshHours"), "at least 1 hour"),
        }
    }
    match &cfg.active_subscription {
        Some(a) => v.check(
            cfg.subscriptions.iter().any(|s| s.id.as_str() == a && s.enabled),
            "/activeSubscription",
            "must name an enabled subscription",
        ),
        None => v.error("/activeSubscription", "choose the subscription for the managed configuration"),
    }
    // Same exclusivity in configuration as in registration: one owner of Mihomo/TPROXY resources.
    let plain = req.config.services.get("mihomo").map(|m| m.get("enabled") != Some(&Value::Bool(false))).unwrap_or(false);
    v.check(!plain, "/enabled", "services.mihomo is also enabled on this node; only one Mihomo owner is allowed");
    if cfg!(target_os = "linux") {
        let d = openclash::detect(Path::new("/"));
        if let Some(reason) = d.reason {
            v.error("/enabled", reason);
        }
        // Never overwrite a user's file that happens to have the managed name.
        let path = managed_path(cfg);
        let owned =
            openclash::Takeover::load(&takeover_path(&req.context.state_dir)).map(|t| t.managed_files.contains(&path)).unwrap_or(false);
        v.check(
            !Path::new(&path).exists() || owned,
            "/configName",
            format!("{path} exists and was not created by this adapter; choose another name"),
        );
    }
    v.finish().err().unwrap_or_default()
}

/// Keys OpenClash rewrites from its own settings when it starts the core; any subscription value is dropped
/// so the managed file shows what is really in effect.
pub const OPENCLASH_CONTROLLED_KEYS: &[&str] = &[
    "port",
    "socks-port",
    "redir-port",
    "tproxy-port",
    "mixed-port",
    "allow-lan",
    "bind-address",
    "external-controller",
    "external-controller-tls",
    "external-controller-unix",
    "external-ui",
    "external-ui-url",
    "external-ui-name",
    "secret",
    "dns",
    "tun",
    "iptables",
    "interface-name",
    "routing-mark",
    "ipv6",
];

pub const MANAGED_HEADER: &str = "# Managed by meduza-np-mihomo-openwrt. Ports, controller, DNS and TUN come from OpenClash settings.\n";

/// Builds the managed configuration from a subscription. Returns (yaml, dropped keys).
pub fn managed_config(subscription_yaml: &str) -> Result<(String, Vec<String>), String> {
    let doc: Value = serde_norway::from_str(subscription_yaml).map_err(|e| format!("subscription is not valid YAML: {e}"))?;
    let Value::Object(mut map) = doc else { return Err("subscription is not a YAML mapping".into()) };
    if !map.contains_key("proxies") && !map.contains_key("proxy-providers") {
        return Err("subscription contains no proxies or proxy-providers".into());
    }
    let mut dropped = Vec::new();
    for key in OPENCLASH_CONTROLLED_KEYS {
        if map.remove(*key).is_some() {
            dropped.push((*key).to_string());
        }
    }
    let yaml = serde_norway::to_string(&Value::Object(map)).map_err(|e| e.to_string())?;
    Ok((format!("{MANAGED_HEADER}{yaml}"), dropped))
}

/// Redacts credentials of a configuration for previews.
pub fn redact(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        if o.contains_key("secret") {
            o.insert("secret".into(), Value::String("<redacted>".into()));
        }
        if let Some(proxies) = o.get_mut("proxies").and_then(Value::as_array_mut) {
            for p in proxies.iter_mut().filter_map(Value::as_object_mut) {
                for k in ["password", "uuid", "private-key", "psk", "auth-str", "pre-shared-key"] {
                    if p.contains_key(k) {
                        p.insert(k.into(), Value::String("<redacted>".into()));
                    }
                }
            }
        }
    }
    v
}

pub fn plan(cfg: &OpenClashConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    let libexec = req.context.libexec_dir.trim_end_matches('/');
    let mut subs = Vec::new();
    if cfg.enabled {
        for s in cfg.subscriptions.iter().filter(|s| s.enabled) {
            subs.push(serde_json::json!({"id": s.id, "url": req.secret(&s.url_ref)?.value, "refreshSeconds": s.refresh_secs()}));
        }
    }
    // The wrapper's whole input, secrets included, lives in a 0600 file — not in argv.
    let spec = serde_json::json!({
        "enabled": cfg.enabled,
        "managedPath": managed_path(cfg),
        "options": if cfg.enabled { desired_options(cfg) } else { BTreeMap::new() },
        "subscriptions": subs,
        "active": cfg.active_subscription,
        "restartOnChange": cfg.restart_on_change,
        "dnsIntegration": cfg.dns_integration,
        "downloadProxy": cfg.download_proxy,
        "stateDir": req.context.state_dir,
    });
    let spec_text = serde_json::to_string_pretty(&spec).map_err(|e| PluginError::new("internal", e.to_string()))?;
    // Enabled: a supervised integration loop. Disabled: a one-shot release that restores the takeover.
    let (verb, restart, probes) = if cfg.enabled {
        ("run", RestartPolicy::Always, vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }])
    } else {
        ("release", RestartPolicy::Never, Vec::new())
    };
    Ok(vec![GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        instance: INSTANCE.into(),
        files: vec![PlannedFile {
            path: "mihomo-openwrt/main/spec.json".into(),
            mode: "0600".into(),
            content: spec_text.clone(),
            sensitive: true,
        }],
        processes: vec![ProcessSpec {
            id: INSTANCE.into(),
            provider_slot: None,
            program: Some(format!("{libexec}/plugins/mihomo-openwrt")),
            argv: vec![
                verb.into(),
                "--spec".into(),
                "${GENERATION_ROOT}/mihomo-openwrt/main/spec.json".into(),
                "--run-dir".into(),
                "${RUN_DIR}".into(),
            ],
            env: Default::default(),
            restart,
            startup: Default::default(),
            stop_timeout_seconds: 10,
            reload_signal: None,
            restart_digest: Some(sha256_hex(spec_text.as_bytes())),
            user: None,
            bound_to: None,
        }],
        resources: Vec::new(),
        depends_on: Vec::new(),
        probes,
        checks: Vec::new(),
        reload: Vec::new(),
    }])
}

#[cfg(test)]
mod tests {
    use super::openclash::*;
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn req(m: Value) -> PluginRequest {
        test_request(
            "plan",
            json!({
                "services": {"mihomo-openwrt": m},
                "secrets": {"sub1": {"kind": "text", "value": "https://sub.example.test/x?token=t"}}
            }),
        )
    }

    fn base() -> Value {
        json!({"subscriptions": [{"id": "main", "urlRef": "sub1"}], "activeSubscription": "main", "mode": "fake-ip"})
    }

    #[test]
    fn plan_runs_wrapper_without_secrets_in_argv() {
        let r = req(base());
        let cfg = load(&r).unwrap().unwrap();
        let errs: Vec<_> = validate(&cfg, &r).into_iter().filter(|e| e.path != "/enabled" && e.path != "/configName").collect();
        assert!(errs.is_empty(), "{errs:?}");
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        let p = &plans[0].processes[0];
        assert_eq!(p.argv[0], "run");
        assert_eq!(p.restart, RestartPolicy::Always);
        assert!(!p.argv.join(" ").contains("token=t"));
        assert!(plans[0].resources.is_empty(), "no second TPROXY: OpenClash owns its rules");
        let spec: Value = serde_json::from_str(&plans[0].files[0].content).unwrap();
        assert_eq!(spec["options"], json!({"config_path": "/etc/openclash/config/meduza-np.yaml", "en_mode": "fake-ip", "enable": "1"}));
        assert!(plans[0].files[0].sensitive);
    }

    #[test]
    fn disabled_plans_one_shot_release() {
        let mut m = base();
        m["enabled"] = json!(false);
        let r = req(m);
        let cfg = load(&r).unwrap().unwrap();
        assert!(validate(&cfg, &r).is_empty());
        let plans = plan(&cfg, &r).unwrap();
        let p = &plans[0].processes[0];
        assert_eq!((p.argv[0].as_str(), p.restart), ("release", RestartPolicy::Never));
        let spec: Value = serde_json::from_str(&plans[0].files[0].content).unwrap();
        assert_eq!(spec["subscriptions"], json!([]), "no secrets when disabled");
    }

    #[test]
    fn validation() {
        let mut m = base();
        m["configName"] = json!("../x");
        m["activeSubscription"] = json!("nope");
        let mut r = req(m);
        r.config.services.insert("mihomo".into(), json!({"mode": "proxy"}));
        let text = format!("{:?}", validate(&load(&r).unwrap().unwrap(), &r));
        for needle in ["lowercase", "enabled subscription", "only one Mihomo owner"] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn managed_config_drops_openclash_controlled_keys() {
        let sub = "mixed-port: 1\nexternal-controller: 0.0.0.0:9999\nsecret: leaked\ndns:\n  enable: true\ntun:\n  enable: true\nproxies:\n  - name: a\n    type: ss\n    server: s.example.test\n    port: 443\n    password: pw\nrules:\n  - MATCH,a\n";
        let (yaml, dropped) = managed_config(sub).unwrap();
        assert!(yaml.starts_with(MANAGED_HEADER));
        for k in ["mixed-port", "external-controller", "secret", "dns", "tun"] {
            assert!(dropped.contains(&k.to_string()), "{k}");
        }
        let v: Value = serde_norway::from_str(&yaml).unwrap();
        assert!(v.get("secret").is_none() && v.get("dns").is_none());
        assert_eq!(v["rules"], json!(["MATCH,a"]));
        assert_eq!(redact(v)["proxies"][0]["password"], json!("<redacted>"));
        assert!(managed_config("rules: []\n").is_err());
    }

    #[test]
    fn version_detection_and_range() {
        assert_eq!(parse_version("0.46.079-beta"), Some((0, 46, 79)));
        assert_eq!(parse_version("v0.45.1"), Some((0, 45, 1)));
        assert_eq!(parse_version("garbage"), None);
        let dir = std::env::temp_dir().join(format!("mnp-oc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!detect(&dir).installed);
        std::fs::create_dir_all(dir.join("etc/init.d")).unwrap();
        std::fs::create_dir_all(dir.join("etc/config")).unwrap();
        std::fs::write(dir.join("etc/init.d/openclash"), "").unwrap();
        std::fs::write(dir.join("etc/config/openclash"), "").unwrap();
        let d = detect(&dir);
        assert!(d.installed && !d.compatible && d.reason.unwrap().contains("cannot determine"));
        std::fs::create_dir_all(dir.join("lib/apk/db")).unwrap();
        std::fs::write(dir.join("lib/apk/db/installed"), "P:busybox\nV:1.36\n\nP:luci-app-openclash\nV:0.46.086-r1\n").unwrap();
        let d = detect(&dir);
        assert!(d.compatible, "{d:?}");
        assert_eq!(d.version.as_deref(), Some("0.46.086-r1"));
        std::fs::create_dir_all(dir.join("usr/lib/opkg/info")).unwrap();
        std::fs::write(dir.join("usr/lib/opkg/info/luci-app-openclash.control"), "Package: luci-app-openclash\nVersion: 0.49.0-beta\n")
            .unwrap();
        let d = detect(&dir);
        assert!(!d.compatible && d.reason.unwrap().contains("outside the tested range"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn takeover_records_before_change_and_restores() {
        let mut uci = MemUci::default();
        uci.values.insert("enable".into(), "0".into());
        uci.values.insert("config_path".into(), "/etc/openclash/config/user.yaml".into());
        uci.values.insert("cn_port".into(), "9090".into());
        let r = req(base());
        let desired = desired_options(&load(&r).unwrap().unwrap());
        let mut t = Takeover::default();
        assert!(apply_options(&mut uci, &t, &desired).is_err(), "no change before the original is recorded");
        let keys: Vec<&str> = desired.keys().map(String::as_str).collect();
        assert!(t.record(&uci, &keys));
        assert!(!t.record(&uci, &keys), "originals are recorded once");
        assert_eq!(apply_options(&mut uci, &t, &desired).unwrap(), 3);
        assert_eq!(apply_options(&mut uci, &t, &desired).unwrap(), 0, "idempotent: no commit/restart");
        assert_eq!(uci.get("config_path").as_deref(), Some("/etc/openclash/config/meduza-np.yaml"));
        // A later re-take must not overwrite the true originals with our own values.
        assert!(!t.record(&uci, &keys));
        assert_eq!(t.restore(&mut uci).unwrap(), 3);
        assert_eq!(uci.get("enable").as_deref(), Some("0"));
        assert_eq!(uci.get("config_path").as_deref(), Some("/etc/openclash/config/user.yaml"));
        assert_eq!(uci.get("en_mode"), None, "an unset option is unset again");
        assert_eq!(uci.get("cn_port").as_deref(), Some("9090"), "unmanaged options untouched");
        assert!(uci.set("cn_port", "1").is_err(), "only managed options are writable");
    }

    #[test]
    fn uci_show_parsing() {
        let m = parse_uci_show("openclash.config=openclash\nopenclash.config.enable='1'\nopenclash.config.dashboard_password='a'\\''b'\nopenclash.other.x='y'\n", "openclash.config");
        assert_eq!(m.get("enable").map(String::as_str), Some("1"));
        assert_eq!(m.get("dashboard_password").map(String::as_str), Some("a'b"));
        assert!(!m.contains_key("x"));
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<OpenClashConfig>(&path).unwrap();
    }
}
