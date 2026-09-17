use std::process::Command;

fn dg(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dg")).args(args).output().unwrap()
}

#[test]
fn help_and_version() {
    let help = dg(&["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("serve"));
    let version = dg(&["--version"]);
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).starts_with("dg "));
}

#[test]
fn invalid_configuration_fails_without_binding() {
    assert!(!dg(&["serve", "--listen", "not-an-address"]).status.success());
    let public = dg(&["serve", "--listen", "0.0.0.0:7432"]);
    assert!(!public.status.success());
    let log: serde_json::Value = serde_json::from_slice(&public.stderr).unwrap();
    assert_eq!(log["fields"]["code"], "invalid_listen_address");
}
