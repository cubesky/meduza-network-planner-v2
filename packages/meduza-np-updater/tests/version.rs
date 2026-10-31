use std::process::Command;

#[test]
fn version_does_not_require_update_configuration() {
    for argument in ["--version", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_meduza-np-updater"))
            .arg(argument)
            .env("MEDUZA_NP_UPDATE_BASE_URL", "invalid-update-url")
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.starts_with(b"meduza-np-updater "));
        assert!(output.stderr.is_empty());
    }
}
