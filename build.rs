fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");

    // Build scripts see feature activation via CARGO_FEATURE_<NAME>, not
    // cfg!(feature = "..."). cfg!() reflects build.rs's own compilation unit.
    let ui_enabled = std::env::var("CARGO_FEATURE_UI").is_ok();
    if !ui_enabled {
        return;
    }

    if std::path::Path::new("frontend/dist/index.html").exists() {
        return;
    }

    for bin in ["node", "npm"] {
        if which(bin).is_none() {
            panic!(
                "the `ui` feature is enabled but `{bin}` was not found on PATH; \
                 install Node.js, or build with `--no-default-features` to skip \
                 the embedded admin UI entirely"
            );
        }
    }

    run("npm", &["ci"]);
    run("npm", &["run", "build"]);
}

fn run(cmd: &str, args: &[&str]) {
    let status = std::process::Command::new(cmd)
        .args(args)
        .current_dir("frontend")
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd} {}`: {e}", args.join(" ")));

    assert!(status.success(), "`{cmd} {}` failed", args.join(" "));
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file().then_some(candidate)
        })
    })
}
