//! JSON Schema generation shared by the protocol and every plugin's `schema/config.json`.

use schemars::JsonSchema;
use serde_json::Value;

/// Generates the schema for `T` with project fix-ups applied, as pretty JSON with a trailing newline.
pub fn generate<T: JsonSchema>() -> String {
    let mut value = schemars::schema_for!(T).to_value();
    fix(&mut value);
    let mut text = serde_json::to_string_pretty(&value).expect("schema serializes");
    text.push('\n');
    text
}

/// ipnet's `IpNet` schema is a `oneOf` whose IPv6 pattern also matches IPv4 text, so a valid IPv4 network
/// would match both branches; `anyOf` accepts the same set without the ambiguity.
pub fn fix(value: &mut Value) {
    if let Some(ipnet) = value.pointer_mut("/$defs/IpNet").and_then(|v| v.as_object_mut()) {
        if let Some(branches) = ipnet.remove("oneOf") {
            ipnet.insert("anyOf".into(), branches);
        }
    }
}

/// Compares `path` with the generated schema; rewrites it when `MEDUZA_NP_UPDATE_SCHEMAS` is set.
/// Returns an error message when stale.
pub fn check_file<T: JsonSchema>(path: &std::path::Path) -> Result<(), String> {
    let text = generate::<T>();
    let current = std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n");
    if current == text {
        return Ok(());
    }
    if std::env::var_os("MEDUZA_NP_UPDATE_SCHEMAS").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, &text).map_err(|e| e.to_string())?;
        return Ok(());
    }
    Err(format!("{} is stale; regenerate with MEDUZA_NP_UPDATE_SCHEMAS=1", path.display()))
}
