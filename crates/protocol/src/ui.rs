//! UI descriptor (§5.3). Layout only: types/defaults/constraints come from the package config schema.
//! Renderers only understand the built-in widgets below and call registered action ids — no code in JSON.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::Validator;

pub const UI_VERSION: u32 = 1;

/// Interface languages renderers support (BCP 47); keys of `UiDescriptor::i18n`.
pub const LOCALES: &[&str] = &["en", "zh-Hans", "zh-Hant"];

/// Widgets every renderer (manager, dashboard, LuCI adapter) must implement.
pub const WIDGETS: &[&str] =
    &["form", "editable-table", "keypair", "secret", "cidr-list", "tag-list", "select", "textarea", "yaml", "status-table", "certificate"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiDescriptor {
    pub ui_version: u32,
    pub package: String,
    pub requires_capability: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Singleton settings (`services.dnsmasq`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingsBlock>,
    /// Multi-instance collections (`services.wireguard.instances`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub collections: Vec<Collection>,
    /// Package-level actions (not bound to one instance).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    /// Read-only status panels bound to report instance kinds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<StatusPanel>,
    /// Translations per interface language. Missing entries fall back to the text as written and to the
    /// humanized field name, so a descriptor stays usable with partial or no translations.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub i18n: BTreeMap<String, UiTranslations>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiTranslations {
    /// Descriptor texts (titles, descriptions, action labels and confirmations) keyed by the text as written.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub text: BTreeMap<String, String>,
    /// Field and column labels keyed by the field name as used in `fields` / `columns`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SettingsBlock {
    pub path: String,
    pub title: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Collection {
    pub path: String,
    pub item_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Field paths; `runtime.*` columns come from the matching report instance.
    pub columns: Vec<String>,
    pub sections: Vec<Section>,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Report instance kind whose entries match items of this collection by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<String>,
    /// Nested collection path relative to the item (e.g. `peers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Show only when the expression holds: `<field> == <json literal>` or `<field> != <json literal>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    #[serde(default)]
    pub advanced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub mutating: bool,
    /// Text shown in the confirmation dialog describing the concrete impact (§6.2 dangerous operations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// Minimum manager role: viewer, operator, admin.
    #[serde(default = "default_role")]
    pub role: String,
    /// Requires recent Passkey re-verification (secret export).
    #[serde(default)]
    pub reauth: bool,
}

fn default_role() -> String {
    "operator".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusPanel {
    pub title: String,
    pub instance_kind: String,
    pub columns: Vec<String>,
}

impl UiDescriptor {
    pub fn validate(&self) -> Validator {
        let mut v = Validator::new();
        v.check(self.ui_version == UI_VERSION, "/uiVersion", "unsupported uiVersion");
        v.check(!self.requires_capability.is_empty(), "/requiresCapability", "required");
        v.check(
            !self.settings.is_empty() || !self.collections.is_empty() || !self.status.is_empty(),
            "",
            "descriptor declares nothing to render",
        );
        for l in self.i18n.keys() {
            v.check(
                LOCALES.contains(&l.as_str()),
                format!("/i18n/{l}"),
                format!("unsupported language (supported: {})", LOCALES.join(", ")),
            );
        }
        let mut action_ids = std::collections::BTreeSet::new();
        let check_action = |v: &mut Validator, path: String, a: &Action, ids: &mut std::collections::BTreeSet<String>| {
            v.check(
                !a.id.is_empty() && a.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                format!("{path}/id"),
                "action ids are lowercase words separated by '-'",
            );
            v.check(ids.insert(a.id.clone()), format!("{path}/id"), "duplicate action id");
            v.check(
                matches!(a.role.as_str(), "viewer" | "operator" | "admin"),
                format!("{path}/role"),
                "role must be viewer, operator or admin",
            );
        };
        for (i, a) in self.actions.iter().enumerate() {
            check_action(&mut v, format!("/actions/{i}"), a, &mut action_ids);
        }
        for (ci, c) in self.collections.iter().enumerate() {
            let base = format!("/collections/{ci}");
            v.check(c.path.starts_with("services."), format!("{base}/path"), "collections live under services.");
            for (i, a) in c.actions.iter().enumerate() {
                check_action(&mut v, format!("{base}/actions/{i}"), a, &mut action_ids);
            }
            for (si, s) in c.sections.iter().enumerate() {
                check_section(&mut v, format!("{base}/sections/{si}"), s);
            }
        }
        for (bi, b) in self.settings.iter().enumerate() {
            v.check(b.path.starts_with("services."), format!("/settings/{bi}/path"), "settings live under services.");
            for (si, s) in b.sections.iter().enumerate() {
                check_section(&mut v, format!("/settings/{bi}/sections/{si}"), s);
            }
        }
        v
    }
}

fn check_section(v: &mut Validator, path: String, s: &Section) {
    if let Some(w) = &s.widget {
        v.check(WIDGETS.contains(&w.as_str()), format!("{path}/widget"), format!("unknown widget {w}"));
    }
    v.check(!s.fields.is_empty() || s.path.is_some(), &path, "section needs fields or a nested path");
    if let Some(when) = &s.when {
        v.check(parse_when(when).is_some(), format!("{path}/when"), "expected '<field> == <json>' or '<field> != <json>'");
    }
}

/// Parses a `when` expression into (field, negate, literal).
pub fn parse_when(expr: &str) -> Option<(String, bool, serde_json::Value)> {
    let (field, negate, lit) = if let Some((f, l)) = expr.split_once("!=") {
        (f, true, l)
    } else {
        let (f, l) = expr.split_once("==")?;
        (f, false, l)
    };
    let field = field.trim();
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_') {
        return None;
    }
    Some((field.to_owned(), negate, serde_json::from_str(lit.trim()).ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spec_descriptor_valid() {
        let d: UiDescriptor = serde_json::from_value(json!({
          "uiVersion": 1,
          "package": "meduza-np-wireguard",
          "requiresCapability": "wireguard.interfaces",
          "title": "WireGuard",
          "collections": [{
              "path": "services.wireguard.instances",
              "itemId": "id",
              "columns": ["name", "device", "enabled", "runtime.state", "runtime.latestHandshake"],
              "sections": [
                {"title": "接口", "fields": ["name", "enabled", "device", "addresses", "listenPort", "mtu"]},
                {"title": "密钥", "fields": ["privateKeyRef"], "widget": "keypair"},
                {"title": "Peers", "path": "peers", "widget": "editable-table"}
              ],
              "actions": [
                {"id": "generate-keypair", "label": "生成密钥对", "mutating": true},
                {"id": "restart-instance", "label": "重启", "mutating": true}
              ]
          }]
        }))
        .unwrap();
        assert!(d.validate().is_empty(), "{:?}", d.validate().errors());
    }

    #[test]
    fn when_expressions() {
        assert_eq!(parse_when("mode == \"server\""), Some(("mode".into(), false, json!("server"))));
        assert_eq!(parse_when("tls.enabled != true"), Some(("tls.enabled".into(), true, json!(true))));
        assert_eq!(parse_when("x = 1"), None);
        assert_eq!(parse_when("alert(1) == 1"), None);
    }
}
