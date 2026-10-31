//! Resolves the node's target subset of a verified group (§13.2): only installed (or explicitly selected)
//! packages plus their minimal required dependencies; conflicts and missing platform artifacts block the plan.
//! A normal update never adds business packages: a new required dependency that is not installed blocks.

use std::collections::{BTreeMap, BTreeSet};

use meduza_np_protocol::group::{Artifact, GroupManifest, InstalledSet};
use meduza_np_protocol::manifest::packages_conflict;
use serde::Serialize;

pub const FIXED: &[&str] = &["meduza-np-planner", "meduza-np-updater"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Update what is installed; never add or remove packages.
    Installed,
    /// Fixed components + these packages (+ required dependencies); installed packages outside are removed.
    Explicit(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    Install,
    Upgrade,
    Downgrade,
    Reinstall,
    Keep,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub package: String,
    pub kind: ActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<Artifact>,
    /// Satisfied by the updater built into the base image (no download).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub from_image: bool,
}

impl Action {
    pub fn changes(&self) -> bool {
        !matches!(self.kind, ActionKind::Keep)
    }
    pub fn downloads(&self) -> bool {
        matches!(self.kind, ActionKind::Install | ActionKind::Upgrade | ActionKind::Downgrade | ActionKind::Reinstall) && !self.from_image
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub group_version: String,
    pub group_sequence: u64,
    pub platform: String,
    pub actions: Vec<Action>,
    pub blocked: Vec<String>,
    pub warnings: Vec<String>,
    pub download_bytes: u64,
}

impl Plan {
    pub fn changed(&self) -> bool {
        self.actions.iter().any(Action::changes)
    }
    pub fn is_blocked(&self) -> bool {
        !self.blocked.is_empty()
    }
    pub fn action(&self, package: &str) -> Option<&Action> {
        self.actions.iter().find(|a| a.package == package)
    }
    pub fn target(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter().filter(|a| a.kind != ActionKind::Remove)
    }
}

pub struct ResolveInput<'a> {
    pub group: &'a GroupManifest,
    pub installed: &'a InstalledSet,
    pub selection: &'a Selection,
    pub platform: &'a str,
    pub formats: &'a [&'a str],
    pub image_updater_version: Option<&'a str>,
}

/// Numeric dotted comparison with a lexical tie-break; enough to label upgrade vs downgrade.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |v: &str| -> Vec<u64> { v.trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0)).collect() };
    parts(a).cmp(&parts(b)).then_with(|| a.cmp(b))
}

pub fn pick_artifact<'a>(artifacts: &'a [Artifact], platform: &str, formats: &[&str]) -> Option<&'a Artifact> {
    formats.iter().find_map(|f| {
        artifacts
            .iter()
            .find(|a| a.format == *f && a.platform == platform)
            .or_else(|| artifacts.iter().find(|a| a.format == *f && a.platform == "any"))
    })
}

pub fn resolve(i: &ResolveInput<'_>) -> Plan {
    let g = i.group;
    let mut blocked = Vec::new();
    let mut warnings = Vec::new();
    if !g.platforms.is_empty() && !g.platforms.iter().any(|p| p == i.platform) {
        blocked.push(format!("group {} does not support platform {}", g.group_version, i.platform));
    }

    // 1. wanted roots
    let mut wanted: BTreeSet<String> = match i.selection {
        Selection::Installed => {
            if i.installed.packages.is_empty() {
                blocked.push("nothing is installed; select components explicitly".into());
            }
            i.installed.packages.keys().cloned().collect()
        }
        Selection::Explicit(list) => FIXED.iter().map(|s| s.to_string()).chain(list.iter().cloned()).collect(),
    };

    // 2. dependency closure from the group's declarations
    let mut queue: Vec<String> = wanted.iter().cloned().collect();
    while let Some(name) = queue.pop() {
        let Some(gp) = g.package(&name) else { continue };
        for dep in &gp.requires {
            if wanted.contains(dep) {
                continue;
            }
            match i.selection {
                Selection::Explicit(_) => {
                    wanted.insert(dep.clone());
                    queue.push(dep.clone());
                }
                Selection::Installed => blocked.push(format!(
                    "{} {} requires {dep}, which is not installed (install it explicitly; updates never add packages)",
                    name, gp.version
                )),
            }
        }
    }

    // 3. conflicts (global rules + declared)
    let names: Vec<&String> = wanted.iter().collect();
    for (ai, a) in names.iter().enumerate() {
        for b in &names[ai + 1..] {
            let declared =
                [(a, b), (b, a)].iter().any(|(x, y)| g.package(x).map(|p| p.conflicts.iter().any(|c| c == y.as_str())).unwrap_or(false));
            if declared || packages_conflict(a, b) {
                blocked.push(format!("{a} conflicts with {b}"));
            }
        }
    }

    // 4. per package action
    let mut actions = Vec::new();
    let mut download_bytes = 0;
    for name in &wanted {
        let current = i.installed.packages.get(name);
        let Some(gp) = g.package(name) else {
            blocked.push(format!("{name} is not part of group {}", g.group_version));
            continue;
        };
        let image_ok = name == "meduza-np-updater" && i.image_updater_version == Some(gp.version.as_str());
        let artifact = pick_artifact(&gp.artifacts, i.platform, i.formats);
        if artifact.is_none() && !image_ok {
            blocked.push(format!("{name} {} has no {} artifact for {}", gp.version, i.formats.join("/"), i.platform));
            continue;
        }
        let same_bits = |c: &meduza_np_protocol::group::InstalledPackage| {
            // `system` = installed by the OS package manager; its bits are unknown, the same version counts as kept.
            c.version == gp.version
                && (artifact.map(|a| a.sha256.eq_ignore_ascii_case(&c.sha256)).unwrap_or(false)
                    || (image_ok && c.source == "image")
                    || c.source == "system")
        };
        let (kind, from_image) = match current {
            Some(c) if same_bits(c) => (ActionKind::Keep, c.source == "image"),
            None if image_ok => (ActionKind::Keep, true),
            Some(c) if c.version == gp.version => {
                warnings.push(format!("{name} {}: installed bits differ from the group; reinstalling", gp.version));
                (ActionKind::Reinstall, false)
            }
            Some(c) => match compare_versions(&gp.version, &c.version) {
                std::cmp::Ordering::Less => (ActionKind::Downgrade, false),
                _ => (ActionKind::Upgrade, false),
            },
            None => (ActionKind::Install, false),
        };
        let a = Action {
            package: name.clone(),
            kind,
            from: current.map(|c| c.version.clone()),
            to: Some(gp.version.clone()),
            artifact: if from_image { None } else { artifact.cloned() },
            from_image,
        };
        if a.downloads() {
            download_bytes += a.artifact.as_ref().map(|x| x.size).unwrap_or(0);
        }
        if let Some(m) = &gp.migration {
            if a.changes() {
                warnings.push(format!("{name}: {m}"));
            }
        }
        actions.push(a);
    }

    // 5. removals (explicit selection only; identities, configs and manual files are never touched)
    if matches!(i.selection, Selection::Explicit(_)) {
        for (name, c) in &i.installed.packages {
            if !wanted.contains(name) {
                actions.push(Action {
                    package: name.clone(),
                    kind: ActionKind::Remove,
                    from: Some(c.version.clone()),
                    to: None,
                    artifact: None,
                    from_image: false,
                });
            }
        }
    }
    // planner first, updater last: dependencies before dependents, self-replacement at the very end.
    actions.sort_by_key(|a| (order_rank(&a.package), a.package.clone()));
    blocked.sort();
    blocked.dedup();
    Plan {
        group_version: g.group_version.clone(),
        group_sequence: g.sequence,
        platform: i.platform.to_string(),
        actions,
        blocked,
        warnings,
        download_bytes,
    }
}

fn order_rank(name: &str) -> u8 {
    match name {
        "meduza-np-planner" => 0,
        n if n.ends_with("-pkg") => 1,
        "meduza-np-updater" => 9,
        _ => 5,
    }
}

/// Map of package → version for a plan's target set (used to write the new installed record).
pub fn target_versions(plan: &Plan) -> BTreeMap<String, String> {
    plan.target().filter_map(|a| a.to.clone().map(|v| (a.package.clone(), v))).collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use meduza_np_protocol::group::{Compatibility, GroupPackage, InstalledPackage};
    use meduza_np_protocol::manifest::ApiRange;

    pub fn pkg(name: &str, version: &str, requires: &[&str]) -> GroupPackage {
        GroupPackage {
            name: name.into(),
            version: version.into(),
            fingerprint: "f".repeat(64),
            reused_from_group: None,
            requires: requires.iter().map(|s| s.to_string()).collect(),
            conflicts: vec![],
            artifacts: vec![Artifact {
                platform: "linux/amd64".into(),
                format: "tar.gz".into(),
                url: format!("packages/{name}-{version}.tar.gz"),
                size: 100,
                sha256: format!("{:0>64}", hex::encode(version.as_bytes())),
                oci: None,
            }],
            upstream: None,
            migration: None,
        }
    }

    pub fn group(version: &str, packages: Vec<GroupPackage>) -> GroupManifest {
        let r = ApiRange { min: 1, max: 1 };
        GroupManifest {
            format_version: 1,
            group_version: version.into(),
            sequence: 2,
            source_commit: "c".into(),
            published_at: "2026-10-01T00:00:00Z".into(),
            expires_at: "2099-01-01T00:00:00Z".into(),
            platforms: vec!["linux/amd64".into()],
            compatibility: Compatibility { planner_api: r, manager_api: r, config_schema: r },
            packages,
            notes: None,
        }
    }

    fn installed(list: &[(&str, &str, &str)]) -> InstalledSet {
        InstalledSet {
            format_version: 1,
            backend: "store".into(),
            platform: "linux/amd64".into(),
            packages: list
                .iter()
                .map(|(n, v, s)| {
                    (
                        n.to_string(),
                        InstalledPackage { version: v.to_string(), sha256: s.to_string(), source: "group:v0.1.0".into(), object: None },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn run(g: &GroupManifest, inst: &InstalledSet, sel: Selection, image: Option<&str>) -> Plan {
        resolve(&ResolveInput {
            group: g,
            installed: inst,
            selection: &sel,
            platform: "linux/amd64",
            formats: &["tar.zst", "tar.gz"],
            image_updater_version: image,
        })
    }

    fn sha(v: &str) -> String {
        format!("{:0>64}", hex::encode(v.as_bytes()))
    }

    #[test]
    fn installed_subset_only_and_reuse_keeps() {
        let g = group(
            "v0.2.0",
            vec![
                pkg("meduza-np-planner", "0.2.0", &[]),
                pkg("meduza-np-wireguard", "0.1.0", &["meduza-np-planner"]),
                pkg("meduza-np-frr", "0.2.0", &["meduza-np-planner"]),
                pkg("meduza-np-updater", "0.2.0", &[]),
            ],
        );
        let inst = installed(&[("meduza-np-planner", "0.1.0", "x"), ("meduza-np-wireguard", "0.1.0", &sha("0.1.0"))]);
        let p = run(&g, &inst, Selection::Installed, None);
        assert!(p.blocked.is_empty(), "{:?}", p.blocked);
        let kinds: Vec<(&str, ActionKind)> = p.actions.iter().map(|a| (a.package.as_str(), a.kind)).collect();
        assert_eq!(kinds, vec![("meduza-np-planner", ActionKind::Upgrade), ("meduza-np-wireguard", ActionKind::Keep)]);
        assert_eq!(p.download_bytes, 100, "only the changed package is downloaded; frr/updater are not added");
    }

    #[test]
    fn new_dependency_blocks_normal_update() {
        let g = group(
            "v0.3.0",
            vec![
                pkg("meduza-np-planner", "0.3.0", &[]),
                pkg("meduza-np-mihomo", "0.3.0", &["meduza-np-planner", "meduza-np-mihomo-pkg"]),
                pkg("meduza-np-mihomo-pkg", "0.3.0", &[]),
            ],
        );
        let inst = installed(&[("meduza-np-planner", "0.2.0", "x"), ("meduza-np-mihomo", "0.2.0", "y")]);
        let p = run(&g, &inst, Selection::Installed, None);
        assert!(p.blocked.iter().any(|b| b.contains("requires meduza-np-mihomo-pkg")), "{:?}", p.blocked);
        assert!(p.action("meduza-np-mihomo-pkg").is_none());
    }

    #[test]
    fn explicit_selection_adds_closure_removes_and_uses_image_updater() {
        let g = group(
            "v0.2.0",
            vec![
                pkg("meduza-np-planner", "0.2.0", &[]),
                pkg("meduza-np-updater", "0.2.0", &[]),
                pkg("meduza-np-dnsmasq", "0.2.0", &["meduza-np-planner"]),
                pkg("meduza-np-wireguard", "0.2.0", &["meduza-np-planner"]),
            ],
        );
        let inst = installed(&[("meduza-np-planner", "0.2.0", &sha("0.2.0")), ("meduza-np-wireguard", "0.1.0", "y")]);
        let p = run(&g, &inst, Selection::Explicit(vec!["meduza-np-dnsmasq".into()]), Some("0.2.0"));
        assert!(p.blocked.is_empty(), "{:?}", p.blocked);
        let kinds: Vec<(&str, ActionKind, bool)> = p.actions.iter().map(|a| (a.package.as_str(), a.kind, a.from_image)).collect();
        assert_eq!(
            kinds,
            vec![
                ("meduza-np-planner", ActionKind::Keep, false),
                ("meduza-np-dnsmasq", ActionKind::Install, false),
                ("meduza-np-wireguard", ActionKind::Remove, false),
                ("meduza-np-updater", ActionKind::Keep, true),
            ]
        );
    }

    #[test]
    fn conflicts_platform_and_missing_artifacts_block() {
        let mut g = group(
            "v0.2.0",
            vec![
                pkg("meduza-np-planner", "0.2.0", &[]),
                pkg("meduza-np-updater", "0.2.0", &[]),
                pkg("meduza-np-mihomo", "0.2.0", &[]),
                pkg("meduza-np-mihomo-openwrt", "0.2.0", &[]),
            ],
        );
        g.packages[3].artifacts.clear();
        let p = run(
            &g,
            &InstalledSet::default(),
            Selection::Explicit(vec!["meduza-np-mihomo".into(), "meduza-np-mihomo-openwrt".into()]),
            None,
        );
        assert!(p.blocked.iter().any(|b| b.contains("conflicts")), "{:?}", p.blocked);
        assert!(p.blocked.iter().any(|b| b.contains("mihomo-openwrt 0.2.0 has no")), "{:?}", p.blocked);
        g.platforms = vec!["linux/arm64".into()];
        let p = run(&g, &InstalledSet::default(), Selection::Explicit(vec![]), None);
        assert!(p.blocked.iter().any(|b| b.contains("does not support platform")));
    }

    #[test]
    fn version_order() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("0.10.0", "0.9.9"), Greater);
        assert_ne!(compare_versions("v1.2.3", "1.2.3"), Equal);
        assert_eq!(compare_versions("0.2.0", "0.2.1"), Less);
    }
}
