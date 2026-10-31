//! MosDNS plugin (AGENT.md §9.9) — targets the MosDNS v5 configuration format.
//!
//! The structured model always renders a complete, runnable configuration: domain sets (local, block, ddns,
//! global), local/remote forwarders, cache, the full processing `sequence` and UDP+TCP listeners. Remote rule
//! files are refreshed by the wrapper (`run` mode) into the persistent state directory with temp file +
//! validation + atomic rename and bounded retries; on failure the previous rules stay in use. Every rule file is
//! referenced by a domain set, so a configured list is always in effect.
//!
//! Custom plugins (`pluginsYaml`, D30): an operator-written MosDNS v5 `plugins:` list, for routing pipelines the
//! structured model does not express. It refers to files as `/etc/mosdns/<name>`: the four inline lists are
//! `etcd_local.txt`, `etcd_block.txt`, `etcd_ddns.txt`, `etcd_global.txt`, and rule files by their `fileName`; the
//! plugin replaces each reference with the real path (generation or state directory).

pub mod rules;

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};

use meduza_np_plugin_sdk::{PluginError, PluginRequest};
use meduza_np_protocol::digest::sha256_hex;
use meduza_np_protocol::ids::StableId;
use meduza_np_protocol::plan::{GenerationPlan, PlannedFile, ProbeSpec, ProcessSpec, RestartPolicy};
use meduza_np_protocol::{ValidationError, Validator, PLUGIN_PROTOCOL_VERSION};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SLOT: &str = "mosdns.binary";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(title = "MosDNS")]
pub struct MosdnsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_listen")]
    pub listen: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Domestic / default upstreams (`udp://1.2.3.4`, `tls://dns.example:853`, `https://...`). Not used with
    /// `pluginsYaml`/`advancedYaml`.
    #[serde(default)]
    pub local_upstreams: Vec<String>,
    /// Upstreams for `global` domains, optionally through a proxy.
    #[serde(default)]
    pub remote_upstreams: Vec<String>,
    /// SOCKS5 proxy for remote upstreams (e.g. Mihomo `127.0.0.1:7891`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_socks5: Option<SocketAddr>,
    /// Bootstrap resolver for DoT/DoH upstream names — never the front end itself (§9.10 loop rule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<IpAddr>,
    #[serde(default = "default_cache")]
    pub cache_size: u32,
    #[serde(default)]
    pub domains: DomainSets,
    #[serde(default)]
    pub rule_files: Vec<RuleFile>,
    #[serde(default)]
    pub download: DownloadPolicy,
    /// Advanced: replaces the generated configuration verbatim (listeners must still match listen/port).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advanced_yaml: Option<String>,
    /// Custom pipeline: the entries of MosDNS's `plugins:` list (YAML), replacing the generated pipeline. File
    /// references `/etc/mosdns/<name>` resolve to the inline lists (`etcd_local.txt`, `etcd_block.txt`,
    /// `etcd_ddns.txt`, `etcd_global.txt`) and to rule files by `fileName`. A server must listen on listen:port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins_yaml: Option<String>,
    #[serde(default = "default_probe_name")]
    pub probe_name: String,
}

fn default_true() -> bool {
    true
}
fn default_listen() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}
fn default_port() -> u16 {
    1153
}
fn default_cache() -> u32 {
    4096
}
fn default_probe_name() -> String {
    "example.com".into()
}

/// Inline domain lists by category. `ddns` = dynamic hostnames answered without caching (not a DDNS updater).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DomainSets {
    #[serde(default)]
    pub local: Vec<String>,
    #[serde(default)]
    pub block: Vec<String>,
    #[serde(default)]
    pub ddns: Vec<String>,
    #[serde(default)]
    pub global: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    #[default]
    Local,
    Block,
    Ddns,
    Global,
}

impl Category {
    pub fn tag(self) -> &'static str {
        match self {
            Category::Local => "local_domains",
            Category::Block => "block_domains",
            Category::Ddns => "ddns_domains",
            Category::Global => "global_domains",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RuleFile {
    pub id: StableId,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Domain set the file extends in the generated pipeline (unused with `pluginsYaml`).
    #[serde(default)]
    pub category: Category,
    /// Name `pluginsYaml` refers to it by (`/etc/mosdns/<fileName>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// `https://...` source. Downloaded into the state directory; the URL never becomes a path.
    pub url: String,
    #[serde(default = "default_refresh")]
    pub refresh_hours: u32,
}

fn default_refresh() -> u32 {
    24
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct DownloadPolicy {
    /// `http://127.0.0.1:7890` or `socks5h://127.0.0.1:7891` (e.g. a ready Mihomo).
    pub proxy: Option<String>,
    /// Retry directly when the proxy is unavailable. Off by default (explicit policy, §9.9).
    pub direct_fallback: bool,
    pub max_bytes: u64,
    pub timeout_seconds: u32,
}

impl Default for DownloadPolicy {
    fn default() -> Self {
        Self { proxy: None, direct_fallback: false, max_bytes: 32 * 1024 * 1024, timeout_seconds: 60 }
    }
}

pub fn load(req: &PluginRequest) -> Result<Option<MosdnsConfig>, PluginError> {
    req.service::<MosdnsConfig>("mosdns")
}

fn valid_upstream(u: &str) -> bool {
    ["udp://", "tcp://", "tls://", "https://", "quic://"].iter().any(|p| u.starts_with(p))
        && u.len() > 7
        && !u.contains([' ', '"', '\'', '\n'])
}

pub fn valid_domain_entry(d: &str) -> bool {
    let body = d.strip_prefix("domain:").or_else(|| d.strip_prefix("full:")).or_else(|| d.strip_prefix("keyword:")).unwrap_or(d);
    !body.is_empty() && body.len() <= 253 && body.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._*".contains(&b))
}

/// Front-end listeners from the dnsmasq configuration on the same node (for loop detection).
fn frontend_listeners(req: &PluginRequest) -> Vec<SocketAddr> {
    let Some(d) = req.config.services.get("dnsmasq") else { return Vec::new() };
    let port = d.get("port").and_then(Value::as_u64).unwrap_or(53) as u16;
    d.get("listenAddresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.as_str()?.parse::<IpAddr>().ok())
        .map(|a| SocketAddr::new(a, port))
        .collect()
}

pub fn validate(cfg: &MosdnsConfig, req: &PluginRequest) -> Vec<ValidationError> {
    let mut v = Validator::new();
    if !cfg.enabled {
        return Vec::new();
    }
    let custom = cfg.plugins_yaml.is_some() || cfg.advanced_yaml.is_some();
    v.check(custom || !cfg.local_upstreams.is_empty(), "/localUpstreams", "at least one upstream");
    v.check(!(cfg.plugins_yaml.is_some() && cfg.advanced_yaml.is_some()), "/pluginsYaml", "use pluginsYaml or advancedYaml, not both");
    if let Some(y) = &cfg.plugins_yaml {
        v.check(
            y.contains(&format!("{}:{}", cfg.listen, cfg.port)),
            "/pluginsYaml",
            "a server must listen on listen:port so readiness probes work",
        );
        for name in referenced_files(y) {
            v.check(
                INLINE_FILES.iter().any(|(f, _)| *f == name)
                    || cfg.rule_files.iter().any(|r| r.file_name.as_deref() == Some(name.as_str())),
                "/pluginsYaml",
                format!("/etc/mosdns/{name} is neither an inline list nor a rule file's fileName"),
            );
        }
    }
    let front = frontend_listeners(req);
    for (field, list) in [("localUpstreams", &cfg.local_upstreams), ("remoteUpstreams", &cfg.remote_upstreams)] {
        for (i, u) in list.iter().enumerate() {
            v.check(valid_upstream(u), format!("/{field}/{i}"), "expected udp://, tcp://, tls://, https:// or quic:// upstream");
            // front end → MosDNS → front end would loop forever.
            let looped = front
                .iter()
                .any(|f| u.contains(&format!("//{}:{}", f.ip(), f.port())) || (f.port() == 53 && u == &format!("udp://{}", f.ip())));
            v.check(!looped, format!("/{field}/{i}"), "upstream points back at the dnsmasq front end (resolution loop)");
        }
    }
    v.check(
        !front.contains(&SocketAddr::new(cfg.listen, cfg.port)),
        "/port",
        "MosDNS must listen on its own address/port, not the front end's",
    );
    if let Some(b) = cfg.bootstrap {
        v.check(!front.iter().any(|f| f.ip() == b), "/bootstrap", "bootstrap must not be the front end (it may depend on MosDNS)");
    }
    for (field, list) in
        [("local", &cfg.domains.local), ("block", &cfg.domains.block), ("ddns", &cfg.domains.ddns), ("global", &cfg.domains.global)]
    {
        for (i, d) in list.iter().enumerate() {
            v.check(valid_domain_entry(d), format!("/domains/{field}/{i}"), "invalid domain entry");
        }
    }
    let mut ids = BTreeSet::new();
    for (i, r) in cfg.rule_files.iter().enumerate() {
        v.check(ids.insert(r.id.clone()), format!("/ruleFiles/{i}/id"), "duplicate id");
        v.check(r.url.starts_with("https://") || r.url.starts_with("http://"), format!("/ruleFiles/{i}/url"), "http(s) URL expected");
        v.check(!r.url.contains([' ', '"', '\'', '\n', '`', '$']), format!("/ruleFiles/{i}/url"), "URL contains forbidden characters");
        v.check(r.refresh_hours >= 1, format!("/ruleFiles/{i}/refreshHours"), "at least 1 hour");
        if r.category == Category::Global && !custom {
            v.check(!cfg.remote_upstreams.is_empty(), format!("/ruleFiles/{i}/category"), "global rules need remoteUpstreams");
        }
        if let Some(f) = &r.file_name {
            v.check(
                !f.is_empty() && f.len() <= 128 && f.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._@!".contains(&b)),
                format!("/ruleFiles/{i}/fileName"),
                "plain file name expected",
            );
        }
    }
    if !cfg.domains.global.is_empty() && !custom {
        v.check(!cfg.remote_upstreams.is_empty(), "/remoteUpstreams", "global domains need remote upstreams");
    }
    if let Some(p) = &cfg.download.proxy {
        v.check(
            p.starts_with("http://") || p.starts_with("socks5h://") || p.starts_with("socks5://"),
            "/download/proxy",
            "http:// or socks5h:// proxy URL",
        );
    }
    if let Some(y) = &cfg.advanced_yaml {
        v.check(y.contains("plugins:"), "/advancedYaml", "advanced YAML must define plugins");
        v.check(
            y.contains(&format!("{}:{}", cfg.listen, cfg.port)),
            "/advancedYaml",
            "advanced YAML must listen on listen:port so readiness probes work",
        );
    }
    v.finish().err().unwrap_or_default()
}

fn yaml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn upstream_list(ups: &[String], socks5: Option<SocketAddr>, bootstrap: Option<IpAddr>) -> String {
    let mut out = String::new();
    for u in ups {
        out.push_str(&format!("        - addr: {}\n", yaml_str(u)));
        if let Some(s) = socks5 {
            out.push_str(&format!("          socks5: {}\n", yaml_str(&s.to_string())));
        }
        if let Some(b) = bootstrap {
            out.push_str(&format!("          bootstrap: {}\n", yaml_str(&b.to_string())));
        }
    }
    out
}

/// Names `pluginsYaml` uses for the inline lists, and the domain set each one holds.
pub const INLINE_FILES: [(&str, Category); 4] = [
    ("etcd_local.txt", Category::Local),
    ("etcd_block.txt", Category::Block),
    ("etcd_ddns.txt", Category::Ddns),
    ("etcd_global.txt", Category::Global),
];

/// File names referenced as `/etc/mosdns/<name>` in a plugins list.
pub fn referenced_files(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = yaml;
    while let Some(at) = rest.find("/etc/mosdns/") {
        rest = &rest[at + "/etc/mosdns/".len()..];
        let name: String = rest.chars().take_while(|c| !c.is_whitespace() && !"\"'`,]}".contains(*c)).collect();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Path of a downloaded rule file (state dir, never derived from the URL).
pub fn rule_path(state_dir: &str, id: &str) -> String {
    format!("{}/mosdns/rules/{id}.txt", state_dir.trim_end_matches('/'))
}

pub fn render(cfg: &MosdnsConfig, req: &PluginRequest) -> String {
    if let Some(y) = &cfg.advanced_yaml {
        return format!("# advanced YAML supplied by the operator\n{y}\n");
    }
    let gen = req.gen_path("mosdns/main");
    if let Some(plugins) = &cfg.plugins_yaml {
        let mut y = plugins.clone();
        for name in referenced_files(plugins) {
            let path = INLINE_FILES.iter().find(|(f, _)| *f == name).map(|(_, cat)| format!("{gen}/{}.txt", cat.tag())).or_else(|| {
                cfg.rule_files
                    .iter()
                    .find(|r| r.file_name.as_deref() == Some(name.as_str()))
                    .map(|r| rule_path(&req.context.state_dir, r.id.as_str()))
            });
            if let Some(path) = path {
                y = y.replace(&format!("/etc/mosdns/{name}"), &path);
            }
        }
        return format!(
            "# Meduza NP MosDNS v5 configuration — custom plugins supplied by the operator\nlog:\n  level: info\nplugins:\n{y}\n"
        );
    }
    let listen = format!("{}:{}", cfg.listen, cfg.port);
    let mut y = String::from("# Meduza NP MosDNS v5 configuration — generated\nlog:\n  level: info\nplugins:\n");
    for cat in [Category::Local, Category::Block, Category::Ddns, Category::Global] {
        let mut files = vec![format!("{gen}/{}.txt", cat.tag())];
        for r in cfg.rule_files.iter().filter(|r| r.enabled && r.category == cat) {
            files.push(rule_path(&req.context.state_dir, r.id.as_str()));
        }
        y.push_str(&format!("  - tag: {}\n    type: domain_set\n    args:\n      files:\n", cat.tag()));
        for f in files {
            y.push_str(&format!("        - {}\n", yaml_str(&f)));
        }
    }
    y.push_str(&format!(
        "  - tag: local_forward\n    type: forward\n    args:\n      concurrent: 2\n      upstreams:\n{}",
        upstream_list(&cfg.local_upstreams, None, cfg.bootstrap)
    ));
    if !cfg.remote_upstreams.is_empty() {
        y.push_str(&format!(
            "  - tag: remote_forward\n    type: forward\n    args:\n      concurrent: 2\n      upstreams:\n{}",
            upstream_list(&cfg.remote_upstreams, cfg.remote_socks5, cfg.bootstrap)
        ));
    }
    y.push_str(&format!("  - tag: cache\n    type: cache\n    args:\n      size: {}\n", cfg.cache_size));
    // Complete processing sequence: block → ddns (uncached) → cache → local → global → default.
    y.push_str("  - tag: main_sequence\n    type: sequence\n    args:\n");
    y.push_str("      - matches: qname $block_domains\n        exec: reject 3\n");
    y.push_str("      - matches: qname $ddns_domains\n        exec: $local_forward\n      - matches: has_resp\n        exec: accept\n");
    y.push_str("      - exec: $cache\n      - matches: has_resp\n        exec: accept\n");
    y.push_str("      - matches: qname $local_domains\n        exec: $local_forward\n      - matches: has_resp\n        exec: accept\n");
    if !cfg.remote_upstreams.is_empty() {
        y.push_str(
            "      - matches: qname $global_domains\n        exec: $remote_forward\n      - matches: has_resp\n        exec: accept\n",
        );
    }
    y.push_str("      - exec: $local_forward\n");
    y.push_str(&format!(
        "  - tag: udp_server\n    type: udp_server\n    args:\n      entry: main_sequence\n      listen: {}\n",
        yaml_str(&listen)
    ));
    y.push_str(&format!(
        "  - tag: tcp_server\n    type: tcp_server\n    args:\n      entry: main_sequence\n      listen: {}\n",
        yaml_str(&listen)
    ));
    y
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerSpec {
    pub rules: Vec<RuleFile>,
    pub download: DownloadPolicy,
    pub state_dir: String,
}

pub fn plan(cfg: &MosdnsConfig, req: &PluginRequest) -> Result<Vec<GenerationPlan>, PluginError> {
    if !cfg.enabled {
        return Ok(Vec::new());
    }
    let mosdns = req.provider(SLOT)?.path.clone();
    let yaml = render(cfg, req);
    let mut files =
        vec![PlannedFile { path: "mosdns/main/config.yaml".into(), mode: "0644".into(), content: yaml.clone(), sensitive: false }];
    for (cat, list) in [
        (Category::Local, &cfg.domains.local),
        (Category::Block, &cfg.domains.block),
        (Category::Ddns, &cfg.domains.ddns),
        (Category::Global, &cfg.domains.global),
    ] {
        let mut body = list.join("\n");
        body.push('\n');
        files.push(PlannedFile { path: format!("mosdns/main/{}.txt", cat.tag()), mode: "0644".into(), content: body, sensitive: false });
    }
    let spec = RunnerSpec {
        rules: cfg.rule_files.iter().filter(|r| r.enabled).cloned().collect(),
        download: cfg.download.clone(),
        state_dir: req.context.state_dir.clone(),
    };
    let spec_json = serde_json::to_string(&spec).map_err(|e| PluginError::new("internal", e.to_string()))?;
    let libexec = req.context.libexec_dir.trim_end_matches('/');
    let restart_digest = sha256_hex(
        format!("{yaml}\n{spec_json}\n{}", files.iter().map(|f| sha256_hex(f.content.as_bytes())).collect::<Vec<_>>().join(",")).as_bytes(),
    );
    Ok(vec![GenerationPlan {
        protocol_version: PLUGIN_PROTOCOL_VERSION,
        request_id: req.request_id.clone(),
        instance: "mosdns/main".into(),
        files,
        processes: vec![ProcessSpec {
            id: "mosdns/main".into(),
            provider_slot: None,
            program: Some(format!("{libexec}/plugins/mosdns")),
            argv: vec![
                "run".into(),
                "--mosdns".into(),
                mosdns,
                "--dir".into(),
                "${GENERATION_ROOT}/mosdns/main".into(),
                "--run-dir".into(),
                "${RUN_DIR}".into(),
                "--spec".into(),
                spec_json,
            ],
            env: Default::default(),
            restart: RestartPolicy::Always,
            stop_timeout_seconds: 5,
            reload_signal: None,
            restart_digest: Some(restart_digest),
            user: None,
            bound_to: None,
        }],
        resources: Vec::new(),
        depends_on: Vec::new(),
        probes: vec![ProbeSpec::Plugin { interval_seconds: 30, params: Value::Null }],
        checks: Vec::new(),
        reload: Vec::new(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use meduza_np_plugin_sdk::test_request;
    use serde_json::json;

    fn req(m: Value) -> PluginRequest {
        let mut r = test_request(
            "plan",
            json!({"services": {"mosdns": m, "dnsmasq": {"listenAddresses": ["10.42.1.1"], "port": 53, "fallbackServers": ["1.0.0.1"]}}}),
        );
        r.providers.insert(
            SLOT.into(),
            meduza_np_plugin_sdk::ResolvedProvider { path: "/usr/bin/mosdns".into(), source: "system".into(), version: None },
        );
        r
    }

    #[test]
    fn complete_runnable_template() {
        let r = req(json!({
            "localUpstreams": ["udp://119.29.29.29"], "remoteUpstreams": ["tls://8.8.8.8:853"], "remoteSocks5": "127.0.0.1:7891",
            "domains": {"local": ["domain:lan"], "block": ["domain:ads.example"]},
            "ruleFiles": [{"id": "cn", "category": "local", "url": "https://example.test/cn.txt"}]
        }));
        let cfg = load(&r).unwrap().unwrap();
        assert!(validate(&cfg, &r).is_empty(), "{:?}", validate(&cfg, &r));
        let y = render(&cfg, &r);
        for needle in [
            "type: domain_set",
            "- \"/var/lib/meduza-np/mosdns/rules/cn.txt\"",
            "tag: local_forward",
            "socks5: \"127.0.0.1:7891\"",
            "tag: main_sequence",
            "exec: reject 3",
            "exec: $cache",
            "matches: qname $global_domains",
            "type: udp_server",
            "type: tcp_server",
            "listen: \"127.0.0.1:1153\"",
        ] {
            assert!(y.contains(needle), "missing {needle}\n{y}");
        }
        let plans = plan(&cfg, &r).unwrap();
        assert!(plans[0].validate().is_empty(), "{:?}", plans[0].validate().errors());
        assert_eq!(plans[0].files.iter().find(|f| f.path.ends_with("local_domains.txt")).unwrap().content, "domain:lan\n");
    }

    #[test]
    fn loops_and_bad_entries_rejected() {
        let r = req(json!({
            "port": 53, "listen": "10.42.1.1", "localUpstreams": ["udp://10.42.1.1"], "bootstrap": "10.42.1.1",
            "domains": {"global": ["bad domain"]},
            "ruleFiles": [{"id": "x", "category": "global", "url": "file:///etc/passwd"}]
        }));
        let text = format!("{:?}", validate(&load(&r).unwrap().unwrap(), &r));
        for needle in [
            "resolution loop",
            "not the front end's",
            "bootstrap must not",
            "invalid domain entry",
            "http(s) URL",
            "global rules need remoteUpstreams",
        ] {
            assert!(text.contains(needle), "missing {needle}: {text}");
        }
    }

    #[test]
    fn config_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema/config.json");
        meduza_np_protocol::schema::check_file::<MosdnsConfig>(&path).unwrap();
    }

    #[test]
    fn custom_plugins_resolve_file_references() {
        let plugins = "  - tag: geo\n    type: domain_set\n    args:\n      files: [\"/etc/mosdns/geosite-cn.txt\"]\n  - tag: main\n    type: sequence\n    args:\n      - matches: qname &/etc/mosdns/etcd_local.txt\n        exec: reject\n  - tag: server\n    type: udp_server\n    args:\n      entry: main\n      listen: 0.0.0.0:1153\n";
        let cfg_json = serde_json::json!({
            "listen": "0.0.0.0", "port": 1153, "pluginsYaml": plugins,
            "domains": {"local": ["domain:example.cn"]},
            "ruleFiles": [{"id": "geosite-cn", "fileName": "geosite-cn.txt", "url": "https://rules.example.test/cn"}]
        });
        let cfg: MosdnsConfig = serde_json::from_value(cfg_json.clone()).unwrap();
        assert!(cfg.local_upstreams.is_empty() && cfg.rule_files[0].category == Category::Local, "defaults for custom mode");
        assert_eq!(referenced_files(plugins), ["geosite-cn.txt", "etcd_local.txt"]);
        let r = test_request("plan", serde_json::json!({"services": {"mosdns": cfg_json}}));
        let y = render(&cfg, &r);
        assert!(!y.contains("/etc/mosdns/"), "{y}");
        assert!(y.contains(&rule_path(&r.context.state_dir, "geosite-cn")), "{y}");
        assert!(y.contains("local_domains.txt"), "{y}");
        assert!(y.starts_with("# Meduza NP MosDNS v5 configuration — custom plugins") && y.contains("\nplugins:\n  - tag: geo"), "{y}");
        assert!(validate(&cfg, &r).iter().all(|e| e.path != "/pluginsYaml" && e.path != "/localUpstreams"), "{:?}", validate(&cfg, &r));
        let mut bad = cfg.clone();
        bad.plugins_yaml = Some(plugins.replace("geosite-cn.txt", "missing.txt"));
        assert!(validate(&bad, &r).iter().any(|e| e.path == "/pluginsYaml" && e.message.contains("missing.txt")));
    }
}
