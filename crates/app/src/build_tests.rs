//! Exercise the actual build script with isolated environment and library paths.
use std::{path::PathBuf, process::Command};

#[test]
fn native_build_script_uses_out_dir_and_rejects_bad_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = dir.path().join("native-build-script");
    let compiled = Command::new("rustc")
        .args(["--edition=2024"])
        .arg(manifest.join("build.rs"))
        .arg("-o")
        .arg(&script)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let lightgbm = dir.path().join("custom lightgbm/lib");
    std::fs::create_dir_all(&lightgbm).unwrap();
    std::fs::write(lightgbm.join("lib_lightgbm.dylib"), []).unwrap();
    let wrong = dir
        .path()
        .join("target/duckdb-download/x86_64-apple-darwin/old");
    std::fs::create_dir_all(&wrong).unwrap();
    std::fs::write(wrong.join("libduckdb.dylib"), []).unwrap();
    let run = |profile: &str, override_dir: &str| {
        Command::new(&script)
            .current_dir(&manifest)
            .env("CARGO_MANIFEST_DIR", &manifest)
            .env("CARGO_CFG_TARGET_OS", "macos")
            .env("TARGET", "aarch64-apple-darwin")
            .env("CARGO_TARGET_DIR", "target")
            .env(
                "OUT_DIR",
                dir.path().join(profile).join("build/app-hash/out"),
            )
            .env("DUCKDB_LIB_DIR", override_dir)
            .env("LIGHTGBM_LIB_DIR", &lightgbm)
            .output()
            .unwrap()
    };
    for profile in ["target/debug", "target/aarch64-apple-darwin/release"] {
        let output = run(profile, "");
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains(&format!(
                "cargo:rustc-link-search=native={}",
                dir.path().join(profile).join("deps").display()
            )),
            "{text}"
        );
        assert!(!text.contains("duckdb-download"), "{text}");
        assert!(text.contains("@executable_path/deps"));
        assert!(!text.contains(&lightgbm.display().to_string()));
    }
    let output = run("target/debug", dir.path().to_str().unwrap());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid DUCKDB_LIB_DIR"));
}
