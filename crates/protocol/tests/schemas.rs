//! `schemas/*.json` are generated from the Rust DTOs so the TypeScript manager validates exactly what nodes
//! parse. Run with `MEDUZA_NP_UPDATE_SCHEMAS=1 cargo test -p meduza-np-protocol --test schemas` to regenerate.

use std::path::PathBuf;

use meduza_np_protocol::{access, api, config, events, group, local, manifest, plan, report, ui};
use schemars::{schema_for, Schema};

fn schemas() -> Vec<(&'static str, Schema)> {
    vec![
        ("node-config", schema_for!(config::NodeConfig)),
        ("node-report", schema_for!(report::NodeReport)),
        ("sync-event", schema_for!(events::SyncEvent)),
        ("stream-hello", schema_for!(events::StreamHello)),
        ("stream-welcome", schema_for!(events::StreamWelcome)),
        ("package-manifest", schema_for!(manifest::PackageManifest)),
        ("ui-descriptor", schema_for!(ui::UiDescriptor)),
        ("generation-plan", schema_for!(plan::GenerationPlan)),
        ("group-manifest", schema_for!(group::GroupManifest)),
        ("channel-pointer", schema_for!(group::ChannelPointer)),
        ("signature-file", schema_for!(group::SignatureFile)),
        ("trust-store", schema_for!(group::TrustStore)),
        ("installed-set", schema_for!(group::InstalledSet)),
        ("access-auth-request", schema_for!(access::AccessAuthRequest)),
        ("access-auth-response", schema_for!(access::AccessAuthResponse)),
        ("enroll-request", schema_for!(api::EnrollRequest)),
        ("enroll-response", schema_for!(api::EnrollResponse)),
        ("session-challenge-request", schema_for!(api::ChallengeRequest)),
        ("session-challenge-response", schema_for!(api::ChallengeResponse)),
        ("session-request", schema_for!(api::SessionRequest)),
        ("session-response", schema_for!(api::SessionResponse)),
        ("config-envelope", schema_for!(api::ConfigEnvelope)),
        ("ack", schema_for!(api::Ack)),
        ("job", schema_for!(api::Job)),
        ("job-result", schema_for!(api::JobResult)),
        ("local-request", schema_for!(local::LocalRequest)),
        ("local-response", schema_for!(local::LocalResponse)),
        ("maintenance-request", schema_for!(local::MaintenanceRequest)),
    ]
}

#[test]
fn schemas_up_to_date() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../schemas");
    let update = std::env::var_os("MEDUZA_NP_UPDATE_SCHEMAS").is_some();
    let mut stale = Vec::new();
    for (name, schema) in schemas() {
        let path = dir.join(format!("{name}.json"));
        let mut value = schema.to_value();
        meduza_np_protocol::schema::fix(&mut value);
        let mut text = serde_json::to_string_pretty(&value).unwrap();
        text.push('\n');
        let current = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
        if current != text {
            if update {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(&path, &text).unwrap();
            } else {
                stale.push(name);
            }
        }
    }
    assert!(stale.is_empty(), "stale schemas {stale:?}; regenerate with MEDUZA_NP_UPDATE_SCHEMAS=1");
}
