#![cfg(unix)]
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

#[test]
fn real_plugin_executes_only_fixed_tool_arguments_and_returns_diagnostic_failures_as_results() {
    let root = std::env::temp_dir().join(format!("mnp-lg-tools-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for name in ["ping", "mtr", "iperf3", "vtysh"] {
        let script = root.join(name);
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' \"$@\"\nprintf 'diagnostic stderr' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let invoke = |operation: &str, target: &str| {
        let request = json!({"protocolVersion":1,"requestId":"test","operation":"action",
            "config":{"schemaVersion":1,"revision":"rev-000001","node":{"id":"test"}},
            "context":{"generationRoot":"/tmp","stateDir":"/tmp","runDir":"/tmp","libexecDir":"/tmp"},
            "params":{"action":"query","operation":operation,"target":target}});
        let mut child = Command::new(env!("CARGO_BIN_EXE_meduza-np-lookingglass"))
            .arg("action")
            .env("PATH", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    for operation in ["ping", "mtr", "iperf", "bgp"] {
        let target = if operation == "bgp" { "AS65000" } else { "10.0.0.1" };
        let result = invoke(operation, target);
        assert_eq!(result["ok"], true, "{result}");
        let results = result["result"]["results"].as_array().unwrap();
        assert_eq!(results.len(), if operation == "bgp" { 2 } else { 1 });
        for output in results {
            assert_eq!(output["exitCode"], 1);
            assert_eq!(output["stderr"], "diagnostic stderr");
            assert_eq!(output["timedOut"], false);
            assert!(output["stdout"].as_str().unwrap().contains(if operation == "bgp" { "regexp _65000_" } else { target }));
        }
    }
    assert_eq!(invoke("ping", "10.0.0.1;touch /tmp/not-allowed")["error"]["code"], "invalid-params");
    std::fs::remove_dir_all(root).unwrap();
}
