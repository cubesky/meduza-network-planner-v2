use std::process::Command;

#[test]
fn version_does_not_start_the_server_or_require_runtime_configuration() {
    for argument in ["--version", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_meduza-np-dashboard"))
            .arg(argument)
            .env("MEDUZA_NP_DASHBOARD_LISTEN", "invalid-listen-address")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.starts_with(b"meduza-np-dashboard "));
        assert!(output.stderr.is_empty());
    }
}
