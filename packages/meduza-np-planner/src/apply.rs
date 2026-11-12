//! Configuration apply engine (§11.1):
//!
//! received → validate (schema/capability/provider) → render plans via plugins → conflict & ownership checks →
//! persist pending journal → stage files + native syntax checks → ordered apply with timeouts → startup probes →
//! commit last successful state.
//!
//! [`Engine::prepare`] has no side effects (manager "validated" stage). [`Engine::commit`] changes the system and
//! either commits or reports failure; the caller then re-applies the last good configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meduza_np_plugin_sdk::{PlanResult, Platform, PluginContext, PluginRequest, ValidateResult};
use meduza_np_protocol::api::InstanceOutcome;
use meduza_np_protocol::config::{NodeConfig, SecretValue};
use meduza_np_protocol::digest::{canonical_digest, sha256_hex};
use meduza_np_protocol::manifest::PackageRole;
use meduza_np_protocol::plan::{CommandSpec, GenerationPlan, NetworkResource, ProcessStartupPolicy};
use meduza_np_protocol::{ValidationError, PLUGIN_PROTOCOL_VERSION};
use meduza_np_runtime::fsutil;
use meduza_np_runtime::jitter::{jittered_period, OsJitter};
use meduza_np_runtime::journal::{diff_resources, ApplyStage, OwnedSet, OwnershipJournal, PendingApply};
use meduza_np_runtime::paths::NodePaths;
use serde_json::Value;

use crate::keystore::KeyStore;
use crate::netbackend::NetBackend;
use crate::plugins::PluginRunner;
use crate::registry::Registry;
use crate::state::{save_last_good, AppliedInstance, AppliedState, InstanceApplyError};
use crate::supervisor::{LaunchSpec, Supervisor};

#[derive(Debug, Clone, PartialEq)]
pub struct Rejection {
    /// `invalid-config`, `missing-capability`, `feature-unavailable`, `plan-conflict`, `plugin-failed`.
    pub code: String,
    pub message: String,
    pub details: Vec<ValidationError>,
}

impl Rejection {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), details: Vec::new() }
    }

    /// The message followed by the details (`path: message; …`), as acked and reported in `lastError`.
    pub fn summary(&self) -> String {
        let details = self.details.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("; ");
        if details.is_empty() {
            self.message.clone()
        } else {
            format!("{}: {details}", self.message)
        }
    }
}

/// Plans for one configuration, validated and conflict free; nothing has been changed yet.
pub struct Prepared {
    pub revision: String,
    pub digest: String,
    pub raw: Value,
    pub plans: Vec<(String, GenerationPlan)>,
    pub warnings: Vec<String>,
    /// Enabled features this revision applies without, because they wait for a package, binary or key (D20, D34,
    /// D28). Recorded with the last good configuration (D47).
    pub held_back: Vec<String>,
    /// Commit leaves the saved last good configuration as it is (a restore that had to drop features, D47).
    pub keep_last_good: bool,
    pub instance_errors: BTreeMap<String, InstanceApplyError>,
    /// A bounded retry touches only these instances; healthy instances keep their running processes.
    pub retry_instances: Option<BTreeSet<String>>,
}

pub fn isolated_tunnel(id: &str) -> bool {
    id.split_once('/')
        .is_some_and(|(kind, id)| matches!(kind, "wireguard" | "openvpn") && meduza_np_protocol::ids::validate_stable_id(id).is_ok())
}

fn tunnel_kind(short: &str, value: &Value) -> &'static str {
    if short == "wireguard" {
        "wireguard-interface"
    } else if value.get("kind").and_then(Value::as_str) == Some("access") {
        "openvpn-access"
    } else {
        "openvpn-site2site"
    }
}

fn plan_digest(plan: &GenerationPlan) -> String {
    let files = sha256_hex(
        serde_json::to_string(&plan.files.iter().map(|f| (&f.path, &f.mode, sha256_hex(f.content.as_bytes()))).collect::<Vec<_>>())
            .unwrap_or_default()
            .as_bytes(),
    );
    canonical_digest(&serde_json::json!({"files":files,"processes":plan.processes,"resources":plan.resources})).unwrap_or_default()
}

#[derive(Debug)]
pub struct Failure {
    pub code: String,
    pub message: String,
    pub instances: Vec<InstanceOutcome>,
}

pub struct Engine {
    pub paths: NodePaths,
    pub runner: PluginRunner,
    pub supervisor: Supervisor,
    pub net: Arc<dyn NetBackend>,
    pub keystore: KeyStore,
    /// How long a started process must stay up before the apply counts as successful.
    pub settle: Duration,
    pub settle_deadline: Duration,
    /// Plan digests applied by *this process*: reload hooks (e.g. `wg syncconf`) must run again after a
    /// planner restart or reboot even if the persisted state says nothing changed.
    pub loaded: std::sync::Mutex<BTreeMap<String, String>>,
}

/// Whether a service entry enables anything: `enabled != false`, or any enabled instance.
pub fn service_enabled(v: &Value) -> bool {
    if v.get("enabled") == Some(&Value::Bool(false)) {
        return false;
    }
    match v.get("instances").and_then(Value::as_array) {
        Some(items) => v.get("enabled") == Some(&Value::Bool(true)) || items.iter().any(|i| i.get("enabled") != Some(&Value::Bool(false))),
        None => true,
    }
}

fn collect_refs(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            // Match Manager's secret delivery and plugin activation: a disabled instance/peer may retain
            // references for later use, but its intentionally undelivered keys must not hold back the service.
            if map.get("enabled") == Some(&Value::Bool(false)) {
                return;
            }
            for (k, v) in map {
                if k.ends_with("Ref") {
                    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
                        out.insert(s.to_string());
                    }
                } else if k.ends_with("Refs") {
                    for s in v.as_array().into_iter().flatten().filter_map(Value::as_str) {
                        out.insert(s.to_string());
                    }
                } else {
                    collect_refs(v, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| collect_refs(i, out)),
        _ => {}
    }
}

/// Topological order by `dependsOn`; unknown dependencies are ignored (optional cross-package links).
fn order_plans(plans: &[(String, GenerationPlan)]) -> Result<Vec<usize>, Rejection> {
    let index: BTreeMap<&str, usize> = plans.iter().enumerate().map(|(i, (_, p))| (p.instance.as_str(), i)).collect();
    let mut order = Vec::new();
    let mut state = vec![0u8; plans.len()]; // 0 new, 1 visiting, 2 done
    fn visit(
        i: usize,
        plans: &[(String, GenerationPlan)],
        index: &BTreeMap<&str, usize>,
        state: &mut [u8],
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        match state[i] {
            2 => return Ok(()),
            1 => return Err(plans[i].1.instance.clone()),
            _ => {}
        }
        state[i] = 1;
        for dep in &plans[i].1.depends_on {
            if let Some(&j) = index.get(dep.as_str()) {
                visit(j, plans, index, state, order)?;
            }
        }
        state[i] = 2;
        order.push(i);
        Ok(())
    }
    for i in 0..plans.len() {
        visit(i, plans, &index, &mut state, &mut order)
            .map_err(|inst| Rejection::new("plan-conflict", format!("dependency cycle involving {inst}")))?;
    }
    Ok(order)
}

/// Two resources that cannot both be owned by different instances.
fn resource_key(r: &NetworkResource) -> Option<String> {
    Some(match r {
        NetworkResource::Link { device, .. } => format!("link:{device}"),
        NetworkResource::Address { device, cidr } => format!("addr:{device}:{cidr}"),
        NetworkResource::NftTable { family, name, .. } => format!("nft:{family}:{name}"),
        NetworkResource::Reservation { namespace, value } => format!("res:{namespace}:{value}"),
        NetworkResource::Rule { priority, family, .. } => format!("rule:{family:?}:{priority}"),
        NetworkResource::Route { destination, table, .. } => format!("route:{}:{destination}", table.unwrap_or(254)),
        NetworkResource::Sysctl { .. } | NetworkResource::LinkSettings { .. } => return None,
    })
}

impl Engine {
    fn context(&self, registry: &Registry, platform: &Platform) -> PluginContext {
        PluginContext {
            // Processes reference the stable `current` link, so a new revision does not change argv (§11.1).
            generation_root: self.paths.current_generation_link().to_string_lossy().into_owned(),
            state_dir: self.paths.state.to_string_lossy().into_owned(),
            run_dir: self.paths.run.to_string_lossy().into_owned(),
            libexec_dir: self.paths.libexec.to_string_lossy().into_owned(),
            platform: platform.clone(),
            capabilities: registry.capabilities(),
        }
    }

    /// Adds node-held secrets for references the manager did not deliver.
    fn with_local_secrets(&self, config: &NodeConfig) -> NodeConfig {
        let mut refs = BTreeSet::new();
        collect_refs(&serde_json::to_value(&config.services).unwrap_or_default(), &mut refs);
        let mut out = config.clone();
        for r in refs {
            if let std::collections::btree_map::Entry::Vacant(slot) = out.secrets.entry(r) {
                if let Some(v) = self.keystore.as_secret(slot.key()) {
                    slot.insert(v);
                }
            }
        }
        out
    }

    /// Writes the components a configuration needs for the updater (short names, sorted); rewritten only when the
    /// list changes, so the updater sees a change as an operator request and installs at once.
    ///
    /// Only a newly received revision (the desired state) replaces the list (`merge = false`). Restoring the last
    /// good configuration (startup, failed apply, rescan) merges into it: otherwise a revision whose new feature
    /// fails would drop that package, the updater would remove it, the retried revision would want it again — an
    /// install/remove loop (D46). Validation and diagnostics (`prepare` alone) never touch the list.
    pub fn publish_wanted_components(&self, raw: &Value, merge: bool) {
        let Ok(config) = serde_json::from_value::<NodeConfig>(raw.clone()) else { return };
        if !config.validate().is_empty() {
            return;
        }
        let path = self.paths.wanted_components();
        let mut wanted: BTreeSet<String> = config.services.iter().filter(|(_, v)| service_enabled(v)).map(|(k, _)| k.clone()).collect();
        // Tinc mesh and external links need the routing adapter as well as their transport. Installing it does
        // not invent ASN/peers or start FRR: the manager still compiles its routing configuration separately.
        if meduza_np_protocol::config::needs_routing_package(&config.services) {
            wanted.insert("frr".into());
        }
        if merge {
            let raw = std::fs::read(&path).unwrap_or_default();
            wanted.extend(serde_json::from_slice::<Vec<String>>(&raw).unwrap_or_default());
        }
        let body = serde_json::to_vec(&wanted).unwrap_or_default();
        if std::fs::read(&path).ok().as_deref() != Some(body.as_slice()) {
            if let Err(e) = fsutil::write_atomic(&path, &body, 0o644) {
                warn!("cannot write {}: {e}", path.display());
            }
        }
    }

    pub async fn prepare(&self, raw: &Value, digest: &str, registry: &Registry, platform: &Platform) -> Result<Prepared, Rejection> {
        let parsed: NodeConfig = serde_json::from_value(raw.clone()).map_err(|e| Rejection::new("invalid-config", e.to_string()))?;
        let isolated: Vec<_> = parsed
            .services
            .iter()
            .filter(|(name, value)| matches!(name.as_str(), "wireguard" | "openvpn") && value.get("instances").is_some_and(Value::is_array))
            .collect();
        if isolated.is_empty() {
            return self.prepare_complete(raw, digest, registry, platform, &BTreeSet::new()).await;
        }
        let config = self.with_local_secrets(&parsed);
        // Keep the full configuration context: FRR reads OpenVPN Access pools when originating routes.
        let excluded = isolated.iter().map(|(name, _)| (*name).clone()).collect();
        let mut prepared = self.prepare_complete(raw, digest, registry, platform, &excluded).await?;
        for (short, service) in isolated {
            if !service_enabled(service) {
                continue;
            }
            let package = format!("meduza-np-{short}");
            let mut valid = Vec::new();
            let instances = service["instances"].as_array().unwrap();
            let mut counts = BTreeMap::new();
            for value in instances.iter().filter(|v| v.get("enabled") != Some(&Value::Bool(false))) {
                if let Some(id) = value.get("id").and_then(Value::as_str) {
                    *counts.entry(id).or_insert(0usize) += 1;
                }
            }
            for (index, value) in instances.iter().enumerate().filter(|(_, v)| v.get("enabled") != Some(&Value::Bool(false))) {
                let candidate = value.get("id").and_then(Value::as_str).unwrap_or("");
                let id = crate::tunnels::entry_id(instances, index);
                let instance = format!("{short}/{id}");
                let kind = tunnel_kind(short, value);
                if id != candidate || counts.get(candidate).copied().unwrap_or(0) > 1 {
                    prepared.instance_errors.insert(
                        instance,
                        InstanceApplyError::new(&package, kind, "invalid-config", "instance needs a unique valid stable id", &config),
                    );
                    continue;
                }
                let mut scoped = raw.clone();
                let mut one = service.clone();
                one["instances"] = serde_json::json!([value]);
                scoped["services"] = serde_json::json!({short:one});
                match self.prepare_complete(&scoped, digest, registry, platform, &BTreeSet::new()).await {
                    Ok(single) if !single.held_back.is_empty() => {
                        let message = single.warnings.join("; ");
                        prepared.warnings.extend(single.warnings);
                        prepared
                            .instance_errors
                            .insert(instance, InstanceApplyError::new(&package, kind, "waiting-dependency", &message, &config));
                    }
                    Ok(single) => {
                        prepared.warnings.extend(single.warnings);
                        let plans: Vec<_> = single.plans.into_iter().filter(|(p, _)| p == &package).collect();
                        if plans.len() != 1 || plans[0].1.instance != instance {
                            prepared.instance_errors.insert(
                                instance,
                                InstanceApplyError::new(
                                    &package,
                                    kind,
                                    "invalid-plan",
                                    "plugin did not return exactly its requested instance",
                                    &config,
                                ),
                            );
                        } else {
                            valid.push((id, value.clone(), plans));
                        }
                    }
                    Err(error) => {
                        prepared
                            .instance_errors
                            .insert(instance, InstanceApplyError::new(&package, kind, &error.code, &error.summary(), &config));
                    }
                }
            }
            // Individual parsing must not bypass cross-instance device/port/key collision validation. Only
            // validate the surviving instances; quarantine the indexed offender, then validate the remainder.
            if let Some(pkg) = registry.by_short(short).filter(|p| p.registered()) {
                if let Some(program) = registry.plugin_path(pkg) {
                    while !valid.is_empty() {
                        let mut scoped = config.clone();
                        let mut svc = service.clone();
                        svc["instances"] = Value::Array(valid.iter().map(|(_, v, _)| v.clone()).collect());
                        scoped.services.insert(short.clone(), svc);
                        let mut providers = BTreeMap::new();
                        if let Some(slot) = &pkg.manifest.provider_slot {
                            if let Some(p) = registry.providers.get(slot) {
                                providers.insert(slot.clone(), p.clone());
                            }
                        }
                        let request = PluginRequest {
                            protocol_version: PLUGIN_PROTOCOL_VERSION,
                            request_id: format!("{}-{short}-validate-survivors", parsed.revision),
                            operation: "validate".into(),
                            config: scoped,
                            instance: None,
                            providers,
                            context: self.context(registry, platform),
                            params: Value::Null,
                        };
                        let errors = match self.runner.call(&program, &request).await.and_then(|v| {
                            serde_json::from_value::<ValidateResult>(v)
                                .map_err(|e| crate::plugins::PluginCallError::BadResponse(e.to_string()))
                        }) {
                            Ok(result) => {
                                prepared.warnings.extend(result.warnings);
                                result.errors
                            }
                            Err(error) => vec![ValidationError::new("", error.to_string())],
                        };
                        if errors.is_empty() {
                            break;
                        }
                        let mut rejected = BTreeSet::new();
                        for error in &errors {
                            if let Some(index) = error
                                .path
                                .strip_prefix("/instances/")
                                .and_then(|p| p.split('/').next())
                                .and_then(|p| p.parse::<usize>().ok())
                                .filter(|i| *i < valid.len())
                            {
                                rejected.insert(index);
                            }
                        }
                        if rejected.is_empty() {
                            rejected.extend(0..valid.len());
                        }
                        for index in rejected.into_iter().rev() {
                            let (id, value, _) = valid.remove(index);
                            let message = errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; ");
                            prepared.instance_errors.insert(
                                format!("{short}/{id}"),
                                InstanceApplyError::new(&package, tunnel_kind(short, &value), "invalid-config", &message, &config),
                            );
                        }
                    }
                }
            }
            for (_, value, plans) in valid {
                for plan in plans {
                    let mut proposed = prepared.plans.clone();
                    proposed.push(plan.clone());
                    match self.check_plans(&proposed, registry) {
                        Ok(()) => prepared.plans.push(plan),
                        Err(error) => {
                            prepared.instance_errors.insert(
                                plan.1.instance.clone(),
                                InstanceApplyError::new(&package, tunnel_kind(short, &value), &error.code, &error.summary(), &config),
                            );
                        }
                    }
                }
            }
        }
        Ok(prepared)
    }

    async fn prepare_complete(
        &self,
        raw: &Value,
        digest: &str,
        registry: &Registry,
        platform: &Platform,
        excluded: &BTreeSet<String>,
    ) -> Result<Prepared, Rejection> {
        let config: NodeConfig = serde_json::from_value(raw.clone()).map_err(|e| Rejection::new("invalid-config", e.to_string()))?;
        let v = config.validate();
        if !v.is_empty() {
            return Err(Rejection {
                code: "invalid-config".into(),
                message: "configuration failed validation".into(),
                details: v.errors().to_vec(),
            });
        }
        // Features may be configured before their package is installed (D20): the updater installs what the
        // configuration needs (wanted-components.json, written by the caller: `publish_wanted_components`),
        // otherwise an operator does; until then the feature waits and is reported as a warning instead of
        // rejecting the whole revision. Installed but unusable stays an error.
        let mut warnings = Vec::new();
        let mut preparing = Vec::new();
        let mut held_back = Vec::new();
        for (key, value) in &config.services {
            if excluded.contains(key) || !service_enabled(value) {
                continue;
            }
            let Some(pkg) = registry.by_short(key).filter(|p| p.registered()) else {
                let reason = registry.by_short(key).and_then(|p| p.reason.clone()).unwrap_or_else(|| "not installed".into());
                warnings.push(format!("services.{key}: waiting for meduza-np-{key} ({reason}); applied once it is installed"));
                held_back.push(key.clone());
                continue;
            };
            let caps = crate::registry::enabled_capabilities(&pkg.manifest, value);
            if let Err((code, why)) = registry.availability(pkg, &caps) {
                // A binary prepared at runtime (D34): the preparer is part of this same revision, so rejecting it
                // would never let the binary arrive. Only this feature waits; the provider's rescan re-applies.
                if registry.provider_preparing(pkg) {
                    warnings.push(format!("services.{key}: waiting for its binary ({why}); applied once it is prepared"));
                    preparing.push(key.clone());
                    continue;
                }
                return Err(Rejection::new("feature-unavailable", format!("services.{key}: {code}: {why}")));
            }
        }
        let mut config = self.with_local_secrets(&config);
        for key in &preparing {
            config.services.remove(key);
        }
        held_back.extend(preparing);
        // A key held by this node that it has not generated yet (D28: the manager queues the generation once the
        // package is installed) holds back only the feature that needs it; the rest of the revision applies.
        let waiting: Vec<(String, Vec<String>)> = config
            .services
            .iter()
            .filter(|(key, value)| {
                !excluded.contains(*key) && service_enabled(value) && registry.by_short(key).is_some_and(|p| p.registered())
            })
            .filter_map(|(key, value)| {
                let mut refs = BTreeSet::new();
                collect_refs(value, &mut refs);
                let missing: Vec<String> = refs.into_iter().filter(|r| !config.secrets.contains_key(r)).collect();
                (!missing.is_empty()).then(|| (key.clone(), missing))
            })
            .collect();
        for (key, missing) in waiting {
            let names = missing.join(", ");
            warnings.push(format!("services.{key}: waiting for {names} (generated on this node); applied once it exists"));
            config.services.remove(&key);
            held_back.push(key);
        }
        let context = self.context(registry, platform);
        let mut plans: Vec<(String, GenerationPlan)> = Vec::new();
        for pkg in registry.registered().filter(|p| matches!(p.manifest.role, PackageRole::ServiceAdapter | PackageRole::BinaryProvider)) {
            let short = pkg.short().to_string();
            if excluded.contains(&short) || !config.services.contains_key(&short) {
                continue;
            }
            let Some(program) = registry.plugin_path(pkg) else { continue };
            let mut providers = BTreeMap::new();
            if let Some(slot) = &pkg.manifest.provider_slot {
                if let Some(p) = registry.providers.get(slot) {
                    providers.insert(slot.clone(), p.clone());
                }
            }
            let mut req = PluginRequest {
                protocol_version: PLUGIN_PROTOCOL_VERSION,
                request_id: format!("{}-{short}-validate", config.revision),
                operation: "validate".into(),
                config: config.clone(),
                instance: None,
                providers,
                context: context.clone(),
                params: Value::Null,
            };
            let validated: ValidateResult = self
                .runner
                .call(&program, &req)
                .await
                .and_then(|v| serde_json::from_value(v).map_err(|e| crate::plugins::PluginCallError::BadResponse(e.to_string())))
                .map_err(|e| Rejection::new("plugin-failed", format!("{short} validate: {e}")))?;
            if !validated.errors.is_empty() {
                let details =
                    validated.errors.into_iter().map(|e| ValidationError::new(format!("/services/{short}{}", e.path), e.message)).collect();
                return Err(Rejection { code: "invalid-config".into(), message: format!("services.{short} is invalid"), details });
            }
            warnings.extend(validated.warnings);
            if !pkg.manifest.operations.iter().any(|o| o == "plan") {
                continue;
            }
            req.operation = "plan".into();
            req.request_id = format!("{}-{short}-plan", config.revision);
            let result: PlanResult = self
                .runner
                .call(&program, &req)
                .await
                .and_then(|v| serde_json::from_value(v).map_err(|e| crate::plugins::PluginCallError::BadResponse(e.to_string())))
                .map_err(|e| Rejection::new("plugin-failed", format!("{short} plan: {e}")))?;
            warnings.extend(result.warnings);
            for plan in result.plans {
                plans.push((pkg.manifest.package.clone(), plan));
            }
        }
        // Planner-owned network features (§9.6): mappings and port forwards need nftables.
        if let Some(own) = crate::netplan::plan(&config.networks, &format!("{}-network", config.revision)) {
            if crate::platform::which("nft").is_none() && cfg!(target_os = "linux") {
                return Err(Rejection::new("feature-unavailable", "network mappings / port forwards need nftables (nft) on this node"));
            }
            plans.push(("meduza-np-planner".into(), own));
        }
        self.check_plans(&plans, registry)?;
        held_back.sort();
        Ok(Prepared {
            revision: config.revision.to_string(),
            digest: digest.to_string(),
            raw: raw.clone(),
            plans,
            warnings,
            held_back,
            keep_last_good: false,
            instance_errors: BTreeMap::new(),
            retry_instances: None,
        })
    }

    /// Prepares the last good configuration for a restore (D47). It applied while some features were held back
    /// (`held_back`); once such a feature can be validated it may be rejected, which would make the whole last good
    /// revision invalid although nothing in it changed. Then the revision is prepared again without exactly those
    /// features, the state that was good, and the commit keeps the saved last good as received. The second value is
    /// the rejection of the held-back features, to be reported.
    pub async fn prepare_last_good(
        &self,
        lg: &crate::state::LastGood,
        registry: &Registry,
        platform: &Platform,
    ) -> Result<(Prepared, Option<Rejection>), Rejection> {
        let rejection = match self.prepare(&lg.raw, &lg.digest, registry, platform).await {
            Ok(prepared) => return Ok((prepared, None)),
            Err(r) if lg.held_back.is_empty() => return Err(r),
            Err(r) => r,
        };
        let mut without = lg.raw.clone();
        if let Some(services) = without.get_mut("services").and_then(Value::as_object_mut) {
            for key in &lg.held_back {
                services.remove(key);
            }
        }
        match self.prepare(&without, &lg.digest, registry, platform).await {
            Ok(mut prepared) => {
                prepared.raw = lg.raw.clone();
                prepared.held_back = lg.held_back.clone();
                prepared.keep_last_good = true;
                Ok((prepared, Some(rejection)))
            }
            Err(_) => Err(rejection),
        }
    }

    /// Namespacing, plan validity, duplicate instances/files/processes and cross-instance resource conflicts.
    fn check_plans(&self, plans: &[(String, GenerationPlan)], registry: &Registry) -> Result<(), Rejection> {
        let mut instances = BTreeSet::new();
        let mut files = BTreeSet::new();
        let mut processes = BTreeSet::new();
        let mut owners: BTreeMap<String, String> = BTreeMap::new();
        let mut problems = Vec::new();
        for (package, plan) in plans {
            let short = meduza_np_protocol::manifest::short_name(package);
            let prefix = format!("{short}/");
            for e in plan.validate().errors() {
                problems.push(ValidationError::new(format!("{}{}", plan.instance, e.path), e.message.clone()));
            }
            if !plan.instance.starts_with(&prefix) {
                problems.push(ValidationError::new(&plan.instance, format!("instances of {package} must start with {prefix}")));
            }
            if !instances.insert(plan.instance.clone()) {
                problems.push(ValidationError::new(&plan.instance, "duplicate instance"));
            }
            for f in &plan.files {
                if !f.path.starts_with(&prefix) {
                    problems.push(ValidationError::new(&f.path, format!("files of {package} must live under {prefix}")));
                }
                if !files.insert(f.path.clone()) {
                    problems.push(ValidationError::new(&f.path, "file produced twice"));
                }
            }
            for p in &plan.processes {
                if !processes.insert(p.id.clone()) {
                    problems.push(ValidationError::new(&p.id, "duplicate process id"));
                }
                if let Some(slot) = &p.provider_slot {
                    if !registry.providers.contains_key(slot) {
                        problems.push(ValidationError::new(&p.id, format!("no provider for {slot}")));
                    }
                }
            }
            for r in &plan.resources {
                if let Some(key) = resource_key(r) {
                    if let Some(other) = owners.insert(key.clone(), plan.instance.clone()) {
                        if other != plan.instance {
                            problems.push(ValidationError::new(&plan.instance, format!("{key} is also claimed by {other}")));
                        }
                    }
                }
            }
        }
        order_plans(plans)?;
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Rejection { code: "plan-conflict".into(), message: format!("{} plan problem(s)", problems.len()), details: problems })
        }
    }

    fn expand(&self, arg: &str, generation: &Path, provider: Option<&str>) -> String {
        arg.replace("${GENERATION_ROOT}", &generation.to_string_lossy())
            .replace("${STATE_DIR}", &self.paths.state.to_string_lossy())
            .replace("${RUN_DIR}", &self.paths.run.to_string_lossy())
            .replace("${LIBEXEC_DIR}", &self.paths.libexec.to_string_lossy())
            .replace("${PROVIDER_BIN}", provider.unwrap_or(""))
    }

    fn command_program(&self, c: &CommandSpec, registry: &Registry) -> Result<PathBuf, String> {
        match (&c.provider_slot, &c.program) {
            (Some(slot), _) => {
                registry.providers.get(slot).map(|p| PathBuf::from(&p.path)).ok_or_else(|| format!("no provider for {slot}"))
            }
            (None, Some(p)) => Ok(PathBuf::from(p)),
            _ => Err("command without program".into()),
        }
    }

    async fn run_command(&self, c: &CommandSpec, generation: &Path, registry: &Registry) -> Result<(), String> {
        let program = self.command_program(c, registry)?;
        let provider = c.provider_slot.as_ref().and_then(|s| registry.providers.get(s)).map(|p| p.path.clone());
        let args: Vec<String> = c.argv.iter().map(|a| self.expand(a, generation, provider.as_deref())).collect();
        let fut = tokio::process::Command::new(&program).args(&args).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
        match tokio::time::timeout(Duration::from_secs(c.timeout_seconds.into()), fut).await {
            Err(_) => Err(format!("{} timed out", program.display())),
            Ok(Err(e)) => Err(format!("{}: {e}", program.display())),
            Ok(Ok(out)) if out.status.success() => Ok(()),
            Ok(Ok(out)) => Err(format!(
                "{} failed: {}",
                program.display(),
                String::from_utf8_lossy(if out.stderr.is_empty() { &out.stdout } else { &out.stderr })
                    .trim()
                    .chars()
                    .take(600)
                    .collect::<String>()
            )),
        }
    }

    /// Writes all plan files into `generations/<revision>` (fresh directory, then renamed into place).
    fn stage(&self, prepared: &Prepared) -> std::io::Result<PathBuf> {
        let gens = self.paths.generations();
        fsutil::ensure_dir(&gens, 0o755)?;
        let mut final_dir = gens.join(&prepared.revision);
        if final_dir.exists() {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
            final_dir = gens.join(format!("{}.{stamp:020}", prepared.revision));
        }
        let tmp = gens.join(format!(".{}.staging", prepared.revision));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        for (_, plan) in &prepared.plans {
            for f in &plan.files {
                let path = tmp.join(&f.path);
                let mode = meduza_np_protocol::plan::parse_mode(&f.mode).unwrap_or(0o600);
                if let Some(parent) = path.parent() {
                    fsutil::ensure_dir(parent, 0o755)?;
                }
                fsutil::write_atomic(&path, f.content.as_bytes(), mode)?;
                fsutil::set_mode(&path, mode)?;
            }
        }
        std::fs::rename(&tmp, &final_dir)?;
        fsutil::sync_dir(&gens)?;
        Ok(final_dir)
    }

    fn retain_instance_files(&self, id: &str, generation: &Path) -> std::io::Result<()> {
        if !isolated_tunnel(id) {
            return Ok(());
        }
        let dest = generation.join(id);
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        let source = self.paths.current_generation_link().join(id);
        if source.is_dir() {
            copy_dir(&source, &dest)?;
        }
        Ok(())
    }

    /// Atomically points `current` at `generation`.
    fn switch_current(&self, generation: &Path) -> std::io::Result<()> {
        let link = self.paths.current_generation_link();
        #[cfg(unix)]
        {
            let tmp = link.with_extension("new");
            let _ = std::fs::remove_file(&tmp);
            std::os::unix::fs::symlink(generation, &tmp)?;
            std::fs::rename(&tmp, &link)?;
            fsutil::sync_dir(link.parent().unwrap())
        }
        #[cfg(not(unix))]
        {
            // Development hosts without symlink rights: copy the tree.
            let _ = std::fs::remove_dir_all(&link);
            copy_dir(generation, &link)
        }
    }

    fn prune_generations(&self, keep: &[&str]) {
        let Ok(entries) = std::fs::read_dir(self.paths.generations()) else { return };
        let mut revs: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("rev-"))
            .collect();
        revs.sort();
        // Keep the current generation and one previous one (bounded tmpfs usage, §11.3).
        let mut kept = 0;
        for rev in revs.iter().rev() {
            if keep.contains(&rev.as_str()) || kept < 2 {
                kept += 1;
                continue;
            }
            let _ = std::fs::remove_dir_all(self.paths.generations().join(rev));
        }
    }

    pub async fn commit(&self, prepared: &Prepared, registry: &Registry) -> Result<Vec<InstanceOutcome>, Failure> {
        let fail = |code: &str, message: String, instances: Vec<InstanceOutcome>| Failure { code: code.into(), message, instances };
        let config: NodeConfig = serde_json::from_value(prepared.raw.clone()).map_err(|e| fail("invalid-config", e.to_string(), vec![]))?;
        let config = self.with_local_secrets(&config);
        let mut applied = AppliedState::load(&self.paths).map_err(|e| fail("journal", e.to_string(), vec![]))?;
        let mut instance_errors = prepared.instance_errors.clone();
        if let Some(retrying) = &prepared.retry_instances {
            for (id, error) in &applied.instance_errors {
                if !retrying.contains(id) {
                    instance_errors.insert(id.clone(), error.clone());
                }
            }
        }
        let pending_path = self.paths.pending_journal();
        let mut pending = PendingApply {
            revision: prepared.revision.clone(),
            config_digest: prepared.digest.clone(),
            previous_revision: AppliedState::load(&self.paths).ok().and_then(|a| a.revision),
            stage: ApplyStage::Staged,
            started_at: meduza_np_plugin_sdk::now_rfc3339(),
            completed_instances: Vec::new(),
            error: None,
        };
        pending.save(&pending_path).map_err(|e| fail("journal", e.to_string(), vec![]))?;

        let generation = self.stage(prepared).map_err(|e| fail("stage-failed", e.to_string(), vec![]))?;
        for id in instance_errors.keys() {
            self.retain_instance_files(id, &generation).map_err(|e| fail("stage-failed", e.to_string(), vec![]))?;
        }
        // Native syntax checks against the staged files, before anything changes.
        for (_, plan) in &prepared.plans {
            if prepared.retry_instances.as_ref().is_some_and(|ids| !ids.contains(&plan.instance)) {
                continue;
            }
            for c in &plan.checks {
                if let Err(e) = self.run_command(c, &generation, registry).await {
                    if c.required {
                        if isolated_tunnel(&plan.instance) {
                            let short = plan.instance.split('/').next().unwrap();
                            let value = config
                                .services
                                .get(short)
                                .and_then(|s| s.get("instances"))
                                .and_then(Value::as_array)
                                .and_then(|v| v.iter().find(|v| v.get("id").and_then(Value::as_str) == plan.instance.split('/').nth(1)))
                                .unwrap_or(&Value::Null);
                            instance_errors.insert(
                                plan.instance.clone(),
                                InstanceApplyError::new(
                                    &format!("meduza-np-{short}"),
                                    tunnel_kind(short, value),
                                    "syntax-check-failed",
                                    &e,
                                    &config,
                                ),
                            );
                            self.retain_instance_files(&plan.instance, &generation)
                                .map_err(|e| fail("stage-failed", e.to_string(), vec![]))?;
                            break;
                        }
                        let _ = std::fs::remove_dir_all(&generation);
                        let _ = PendingApply::clear(&pending_path);
                        return Err(fail(
                            "syntax-check-failed",
                            format!("{}: {e}", plan.instance),
                            vec![InstanceOutcome { instance: plan.instance.clone(), ok: false, message: Some(e) }],
                        ));
                    }
                    warn!("{}: optional check failed: {e}", plan.instance);
                }
            }
        }

        pending.stage = ApplyStage::Applying;
        let _ = pending.save(&pending_path);
        let mut ownership = OwnershipJournal::load(&self.paths.ownership()).map_err(|e| fail("journal", e.to_string(), vec![]))?;
        let new_instances: BTreeSet<String> =
            prepared.plans.iter().map(|(_, p)| p.instance.clone()).chain(instance_errors.keys().cloned()).collect();

        // 1. Remove instances that are gone: processes first, then their resources in reverse order.
        let removed: Vec<String> = ownership.instances().filter(|i| !new_instances.contains(i.as_str())).cloned().collect();
        let gone: BTreeSet<String> =
            removed.iter().cloned().chain(applied.instances.keys().filter(|i| !new_instances.contains(i.as_str())).cloned()).collect();
        for inst in &gone {
            let owned = ownership.possibly_owned(inst);
            for p in &owned.processes {
                self.supervisor.stop(p).await;
            }
            if let Some(a) = applied.instances.get(inst.as_str()) {
                for p in a.processes.keys() {
                    self.supervisor.stop(p).await;
                }
            }
            for r in owned.resources.iter().rev() {
                if let Err(e) = self.net.remove(r).await {
                    warn!("{inst}: cleanup {e}");
                }
            }
            let _ = ownership.commit(inst, OwnedSet::default());
            applied.instances.remove(inst.as_str());
            info!("{inst}: removed");
        }

        // 2. New generation becomes current; running daemons keep their open files until restarted/reloaded.
        self.switch_current(&generation).map_err(|e| fail("switch-failed", e.to_string(), vec![]))?;
        let current = self.paths.current_generation_link();

        let order = order_plans(&prepared.plans).map_err(|r| fail(&r.code, r.message, vec![]))?;
        let mut outcomes: Vec<InstanceOutcome> = instance_errors
            .iter()
            .map(|(id, e)| InstanceOutcome { instance: id.clone(), ok: false, message: Some(format!("{}: {}", e.code, e.message)) })
            .collect();
        for i in order {
            let (package, plan) = &prepared.plans[i];
            if instance_errors.contains_key(&plan.instance)
                || prepared.retry_instances.as_ref().is_some_and(|ids| !ids.contains(&plan.instance))
            {
                continue;
            }
            let result: Result<(), Failure> = async {
                // A blocked instance can keep a previously working configuration. Never let another plan
                // claim those resources (or leftovers whose cleanup failed) merely because it has no new plan.
                for owner in ownership.instances().filter(|id| *id != &plan.instance && instance_errors.contains_key(*id)) {
                    let owned = ownership.possibly_owned(owner);
                    if plan
                        .resources
                        .iter()
                        .filter_map(resource_key)
                        .any(|key| owned.resources.iter().filter_map(resource_key).any(|other| other == key))
                    {
                        return Err(fail("plan-conflict", format!("{}: resource retained by {owner}", plan.instance), outcomes.clone()));
                    }
                }
                let previous = ownership.possibly_owned(&plan.instance);
                let (to_remove, to_add) = diff_resources(&previous.resources, &plan.resources);
                let process_ids: BTreeSet<String> = plan.processes.iter().map(|p| p.id.clone()).collect();
                let intent = OwnedSet { resources: to_add.clone(), processes: process_ids.clone() };
                if let Err(e) = ownership.record_intent(&plan.instance, &intent) {
                    return Err(fail("journal", e.to_string(), outcomes.clone()));
                }
                for r in to_remove.iter().rev() {
                    if let Err(e) = self.net.remove(r).await {
                        warn!("{}: {e}", plan.instance);
                    }
                }
                for r in &to_add {
                    if let Err(e) = self.net.add(r).await {
                        outcomes.push(InstanceOutcome { instance: plan.instance.clone(), ok: false, message: Some(e.to_string()) });
                        return Err(fail("network-apply-failed", format!("{}: {e}", plan.instance), outcomes.clone()));
                    }
                }
                // Owned resources the journal lists but the system lost (the fresh network namespace of a re-created
                // planner container, a reboot, `ip link del` by hand) are created again, in plan order; addresses and
                // link settings are re-asserted anyway (e.g. interface recreated by a daemon).
                for r in plan.resources.iter().filter(|r| !to_add.contains(r)) {
                    if matches!(r, NetworkResource::LinkSettings { .. } | NetworkResource::Address { .. }) {
                        if let Err(e) = self.net.add(r).await {
                            warn!("{}: re-assert {e}", plan.instance);
                        }
                    } else if self.net.exists(r).await == Some(false) {
                        info!("{}: recreating lost {}", plan.instance, serde_json::to_string(r).unwrap_or_default());
                        if let Err(e) = self.net.add(r).await {
                            outcomes.push(InstanceOutcome { instance: plan.instance.clone(), ok: false, message: Some(e.to_string()) });
                            return Err(fail("network-apply-failed", format!("{}: {e}", plan.instance), outcomes.clone()));
                        }
                    }
                }

                let file_digest = sha256_hex(
                    serde_json::to_string(
                        &plan.files.iter().map(|f| (&f.path, &f.mode, sha256_hex(f.content.as_bytes()))).collect::<Vec<_>>(),
                    )
                    .unwrap_or_default()
                    .as_bytes(),
                );
                let plan_digest = plan_digest(plan);
                let previous_applied = applied.instances.get(&plan.instance).cloned();
                // Content changed since this planner process last loaded the instance (also true after restarts).
                let content_changed = self.loaded.lock().unwrap().get(&plan.instance) != Some(&plan_digest);
                let mut proc_digests = BTreeMap::new();
                for p in &plan.processes {
                    let provider = p.provider_slot.as_ref().and_then(|s| registry.providers.get(s));
                    let program = match (provider, &p.program) {
                        (Some(prov), _) => PathBuf::from(&prov.path),
                        (None, Some(prog)) => PathBuf::from(prog),
                        _ => return Err(fail("provider-missing", format!("{}: no program", p.id), outcomes.clone())),
                    };
                    let args: Vec<String> = p.argv.iter().map(|a| self.expand(a, &current, provider.map(|x| x.path.as_str()))).collect();
                    // Default restart digest: binary + argv + env + every file of the instance (§11.1: restart only on change).
                    let mut digest = p
                        .restart_digest
                        .clone()
                        .unwrap_or_else(|| sha256_hex(format!("{}\n{:?}\n{:?}\n{file_digest}", program.display(), args, p.env).as_bytes()));
                    if !p.startup.is_required() {
                        // A transition to connection supervision must also replace an unchanged legacy process.
                        digest = sha256_hex(format!("{digest}\n{:?}\n{:?}", p.startup, p.restart).as_bytes());
                    }
                    proc_digests.insert(p.id.clone(), digest.clone());
                    let unchanged = previous_applied.as_ref().and_then(|a| a.processes.get(&p.id)) == Some(&digest);
                    if unchanged && self.supervisor.status(&p.id).is_some() {
                        // Files changed but not the restart-relevant inputs: ask the daemon to reload (e.g. tinc
                        // re-reads hosts on SIGHUP) instead of restarting healthy connections.
                        if content_changed {
                            if let Some(sig) = &p.reload_signal {
                                if !self.supervisor.signal(&p.id, sig) {
                                    warn!("{}: reload signal {sig} could not be delivered", p.id);
                                }
                            }
                        }
                        continue;
                    }
                    self.supervisor
                        .ensure(LaunchSpec {
                            id: p.id.clone(),
                            program,
                            args,
                            env: p.env.clone(),
                            restart: p.restart,
                            startup: p.startup,
                            stop_timeout: Duration::from_secs(p.stop_timeout_seconds.into()),
                            restart_digest: digest,
                            bound_to: p.bound_to.clone(),
                        })
                        .await;
                }
                // Processes of this instance that disappeared from the plan.
                if let Some(prev) = &previous_applied {
                    for old in prev.processes.keys().filter(|id| !process_ids.contains(*id)) {
                        self.supervisor.stop(old).await;
                    }
                }
                // Reload hooks when content changed but no restart was required.
                if content_changed {
                    for c in &plan.reload {
                        if let Err(e) = self.run_command(c, &current, registry).await {
                            if c.required {
                                outcomes.push(InstanceOutcome { instance: plan.instance.clone(), ok: false, message: Some(e.clone()) });
                                return Err(fail("reload-failed", format!("{}: {e}", plan.instance), outcomes.clone()));
                            }
                        }
                    }
                }
                // Local daemons must settle. Connection clients only need successful delivery/exec: a peer's
                // availability is reported separately and must never roll back unrelated local services.
                for p in &plan.processes {
                    let started = match p.startup {
                        ProcessStartupPolicy::Required => self.supervisor.wait_settled(&p.id, self.settle, self.settle_deadline).await,
                        ProcessStartupPolicy::Connection => self.supervisor.wait_started(&p.id, self.settle_deadline).await,
                    };
                    if !started {
                        let why = self.supervisor.status(&p.id).and_then(|s| s.last_exit).unwrap_or_else(|| "did not stay running".into());
                        outcomes.push(InstanceOutcome {
                            instance: plan.instance.clone(),
                            ok: false,
                            message: Some(format!("{}: {why}", p.id)),
                        });
                        return Err(fail("process-failed", format!("{}: {why}", p.id), outcomes.clone()));
                    }
                }
                if let Err(e) = ownership.commit(&plan.instance, OwnedSet { resources: plan.resources.clone(), processes: process_ids }) {
                    return Err(fail("journal", e.to_string(), outcomes.clone()));
                }
                self.loaded.lock().unwrap().insert(plan.instance.clone(), plan_digest.clone());
                applied
                    .instances
                    .insert(plan.instance.clone(), AppliedInstance { package: package.clone(), plan_digest, processes: proc_digests });
                pending.completed_instances.push(plan.instance.clone());
                let _ = pending.save(&pending_path);
                outcomes.push(InstanceOutcome { instance: plan.instance.clone(), ok: true, message: None });
                Ok(())
            }
            .await;
            if let Err(failure) = result {
                if !isolated_tunnel(&plan.instance) || failure.code == "journal" {
                    return Err(failure);
                }
                let short = plan.instance.split('/').next().unwrap();
                let value = config
                    .services
                    .get(short)
                    .and_then(|s| s.get("instances"))
                    .and_then(Value::as_array)
                    .and_then(|v| v.iter().find(|v| v.get("id").and_then(Value::as_str) == plan.instance.split('/').nth(1)))
                    .unwrap_or(&Value::Null);
                let error = InstanceApplyError::new(package, tunnel_kind(short, value), &failure.code, &failure.message, &config);
                warn!("{}: isolated apply failure: {}", plan.instance, error.message);
                let owned = ownership.possibly_owned(&plan.instance);
                let processes: BTreeSet<_> = owned
                    .processes
                    .iter()
                    .cloned()
                    .chain(plan.processes.iter().map(|p| p.id.clone()))
                    .chain(applied.instances.get(&plan.instance).into_iter().flat_map(|a| a.processes.keys().cloned()))
                    .collect();
                for process in processes {
                    self.supervisor.stop(&process).await;
                }
                let mut cleaned = true;
                for resource in owned.resources.iter().rev() {
                    if let Err(e) = self.net.remove(resource).await {
                        warn!("{}: cleanup {e}", plan.instance);
                        cleaned = false;
                    }
                }
                if cleaned {
                    ownership.commit(&plan.instance, OwnedSet::default()).map_err(|e| fail("journal", e.to_string(), outcomes.clone()))?;
                }
                applied.instances.remove(&plan.instance);
                self.loaded.lock().unwrap().remove(&plan.instance);
                outcomes.retain(|o| o.instance != plan.instance);
                outcomes.push(InstanceOutcome {
                    instance: plan.instance.clone(),
                    ok: false,
                    message: Some(format!("{}: {}", error.code, error.message)),
                });
                instance_errors.insert(plan.instance.clone(), error);
            }
        }

        let now = meduza_np_runtime::resources::unix_seconds();
        let mut jitter = OsJitter::new();
        for (id, error) in &mut instance_errors {
            if prepared.retry_instances.as_ref().is_some_and(|ids| !ids.contains(id)) {
                continue;
            }
            let previous = applied.instance_errors.get(id).filter(|_| applied.config_digest.as_deref() == Some(prepared.digest.as_str()));
            error.attempt = previous.map_or(1, |e| e.attempt.saturating_add(1));
            let seconds = (5u64.saturating_mul(1u64 << error.attempt.saturating_sub(1).min(4))).min(60);
            error.retry_at_unix =
                now.saturating_add(jittered_period(&mut jitter, Duration::from_secs(seconds), 0.2).as_secs().clamp(1, 60));
        }
        applied.instance_errors = instance_errors;
        applied.revision = Some(prepared.revision.clone());
        applied.config_digest = Some(prepared.digest.clone());
        applied.save(&self.paths).map_err(|e| fail("journal", e.to_string(), outcomes.clone()))?;
        if !prepared.keep_last_good {
            save_last_good(&self.paths, &prepared.raw, &prepared.digest, &prepared.held_back)
                .map_err(|e| fail("journal", e.to_string(), outcomes.clone()))?;
        }
        let _ = PendingApply::clear(&pending_path);
        self.prune_generations(&[generation.file_name().unwrap().to_str().unwrap()]);
        Ok(outcomes)
    }

    /// Stops everything planner owns for a package that was unregistered (§3.4 uninstall).
    pub async fn remove_package_instances(&self, package: &str) {
        let mut applied = AppliedState::load(&self.paths).unwrap_or_default();
        let Ok(mut ownership) = OwnershipJournal::load(&self.paths.ownership()) else { return };
        let gone: Vec<String> = applied.instances.iter().filter(|(_, a)| a.package == package).map(|(k, _)| k.clone()).collect();
        for inst in gone {
            let owned = ownership.possibly_owned(&inst);
            for p in owned.processes.iter().chain(applied.instances[&inst].processes.keys()) {
                self.supervisor.stop(p).await;
            }
            for r in owned.resources.iter().rev() {
                let _ = self.net.remove(r).await;
            }
            let _ = ownership.commit(&inst, OwnedSet::default());
            applied.instances.remove(&inst);
        }
        let _ = applied.save(&self.paths);
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(from)?.file_type().is_symlink() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "symlink in owned generation"));
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), target)?;
        } else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "non-regular file in owned generation"));
        }
    }
    Ok(())
}

/// Secret material is only needed by plugins; strip it before anything is logged or reported.
pub fn redacted_secret_ids(config: &NodeConfig) -> Vec<String> {
    config.secrets.keys().cloned().collect()
}

pub type SecretMap = BTreeMap<String, SecretValue>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn disabled_openvpn_instances_do_not_require_undelivered_keys() {
        let mut instances: Vec<Value> =
            (0..8).map(|i| json!({"id": format!("active-{i}"), "enabled": true, "staticKeyRef": format!("active-key-{i}")})).collect();
        instances.extend((0..3).map(|i| json!({"id": format!("paused-{i}"), "enabled": false, "staticKeyRef": format!("paused-key-{i}")})));
        let mut refs = BTreeSet::new();
        collect_refs(&json!({"instances": instances}), &mut refs);
        let delivered: BTreeSet<String> = (0..8).map(|i| format!("active-key-{i}")).collect();
        assert_eq!(refs, delivered, "disabled instances' intentionally undelivered keys cannot hold back all eight active tunnels");
    }

    #[test]
    fn secret_refs_follow_enabled_services_and_nested_peers_including_plural_refs() {
        let mut refs = BTreeSet::new();
        collect_refs(
            &json!({
                "disabledService": {"enabled":false, "privateKeyRef":"disabled-service", "nested":{"caRefs":["disabled-ca"]}},
                "activeService": {"instances":[
                    {"privateKeyRef":"default-enabled", "peers":[
                        {"enabled":false,"presharedKeyRef":"disabled-peer","nested":{"keyRef":"disabled-nested"}},
                        {"enabled":true,"presharedKeyRef":"active-peer","caRefs":["active-ca","active-ca"]}
                    ]},
                    {"enabled":false,"privateKeyRef":"disabled-instance"}
                ]}
            }),
            &mut refs,
        );
        assert_eq!(refs, BTreeSet::from(["default-enabled".into(), "active-peer".into(), "active-ca".into()]));
    }
}
