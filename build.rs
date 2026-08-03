fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");

    // Build scripts see feature activation via CARGO_FEATURE_<NAME>, not
    // cfg!(feature = "..."). cfg!() reflects build.rs's own compilation unit.
    let ui_enabled = std::env::var("CARGO_FEATURE_UI").is_ok();
    if !ui_enabled {
        return;
    }

    if dist_is_up_to_date() {
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

/// The plain `frontend/dist/index.html` existence check this replaced
/// permanently skipped rebuilding the embedded UI the moment `dist/` was
/// built once: `cargo:rerun-if-changed` correctly re-invokes build.rs on a
/// frontend edit, but build.rs itself then no-op'd because the *old* dist
/// still existed on disk - so `cargo build --release` never picked up a
/// frontend change again unless `frontend/dist` was deleted by hand. Compare
/// mtimes instead: rebuild whenever any watched input is newer than the
/// dist output.
fn dist_is_up_to_date() -> bool {
    let dist_index = std::path::Path::new("frontend/dist/index.html");
    let dist_mtime = match dist_index.metadata().and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let mut watched = vec![std::path::PathBuf::from("frontend/package.json")];
    collect_files("frontend/src", &mut watched);

    watched.iter().all(|path| {
        path.metadata()
            .and_then(|m| m.modified())
            .map(|mtime| mtime <= dist_mtime)
            .unwrap_or(false)
    })
}

fn collect_files(dir: &str, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(path.to_str().unwrap_or_default(), out);
        } else {
            out.push(path);
        }
    }
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(bin);
            candidate.is_file().then_some(candidate)
        })
    })
}
