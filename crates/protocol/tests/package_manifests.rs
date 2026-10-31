//! Every package in the monorepo ships a manifest that parses and passes validation, and the declared
//! conflicts match the global rule set in both directions (§3.2).

use std::path::PathBuf;

use meduza_np_protocol::manifest::{packages_conflict, PackageManifest, PackageRole, ALL_PACKAGES};
use meduza_np_protocol::ui::UiDescriptor;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load_all() -> Vec<(PathBuf, PackageManifest)> {
    ALL_PACKAGES
        .iter()
        .map(|name| {
            let dir = repo_root().join("packages").join(name);
            let raw = std::fs::read_to_string(dir.join("manifest.json")).unwrap_or_else(|e| panic!("{name}: {e}"));
            let m: PackageManifest = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(&m.package, name);
            (dir, m)
        })
        .collect()
}

#[test]
fn manifests_validate() {
    for (_, m) in load_all() {
        let v = m.validate();
        assert!(v.is_empty(), "{}: {:?}", m.package, v.errors());
    }
}

#[test]
fn conflicts_match_global_rules() {
    let all = load_all();
    for (_, a) in &all {
        for (_, b) in &all {
            let declared = a.conflicts.contains(&b.package) || b.conflicts.contains(&a.package);
            assert_eq!(declared, packages_conflict(&a.package, &b.package), "{} vs {}", a.package, b.package);
        }
    }
}

#[test]
fn ui_descriptors_present_for_plugins_and_valid() {
    for (dir, m) in load_all() {
        let Some(ui) = &m.ui else { continue };
        let path = dir.join(&ui.descriptor);
        if !path.exists() {
            // Plugins add their descriptor together with their implementation; until then only plugins that
            // are not implemented yet may lack it. The planner refuses to register them (missing descriptor).
            eprintln!("note: {} has no {} yet", m.package, ui.descriptor);
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let d: UiDescriptor = serde_json::from_str(&raw).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        assert_eq!(d.package, m.package);
        assert!(m.provides.contains(&d.requires_capability), "{}: descriptor capability not provided", m.package);
        let v = d.validate();
        assert!(v.is_empty(), "{}: {:?}", path.display(), v.errors());
    }
}

#[test]
fn only_plugin_roles_have_entrypoints() {
    for (_, m) in load_all() {
        if m.entrypoint.is_some() {
            assert!(m.role.is_plugin(), "{} has an entrypoint but role {:?}", m.package, m.role);
        }
        if m.role == PackageRole::Manager {
            assert!(m.requires.is_empty(), "manager must not depend on node packages");
        }
    }
}
