use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    // Run in a scratch dir so an accidental boot could not touch a real DB.
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_1router"))
        .args(args)
        .current_dir(dir.path())
        .env("ROUTER_LISTEN_ADDR", "127.0.0.1:0")
        .output()
        .unwrap()
}

#[test]
fn version_prints_and_exits() {
    let out = run(&["--version"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("1router "), "{text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_usage_and_env() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("USAGE") && text.contains("ROUTER_LISTEN_ADDR"), "{text}");
    }
}

#[test]
fn unknown_argument_fails_without_booting() {
    let out = run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--help"));
}
