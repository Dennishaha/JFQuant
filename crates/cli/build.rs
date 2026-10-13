#[path = "../app/build_support.rs"]
mod build_support;
#[path = "../ml/native_paths.rs"]
mod native_paths;
use std::{env, path::PathBuf};
fn main() {
    println!("cargo:rerun-if-changed=../ml/native_paths.rs");
    println!("cargo:rerun-if-changed=../app/build_support.rs");
    for variable in ["DUCKDB_LIB_DIR", "LIGHTGBM_LIB_DIR", "HOME", "PATH"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let duckdb_name = match target_os.as_str() {
        "macos" => "libduckdb.dylib",
        "windows" => "duckdb.dll",
        _ => "libduckdb.so",
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    // libduckdb-sys copies the selected dependency's library into this profile's
    // deps directory. Emit its path even before a cold download finishes.
    let duckdb =
        native_paths::configured_dir(env::var_os("DUCKDB_LIB_DIR").as_deref(), duckdb_name)
            .unwrap_or_else(|error| panic!("invalid DUCKDB_LIB_DIR: {error}"))
            .unwrap_or_else(|| build_support::profile_deps(&out).expect("Cargo profile directory"));
    println!("cargo:rustc-link-search=native={}", duckdb.display());
    emit_rpath(&duckdb.display().to_string(), &target_os);
    if let Some(lightgbm) = native_paths::lightgbm_dir(&target_os) {
        emit_rpath(&lightgbm.display().to_string(), &target_os);
    }
    if target_os == "macos" {
        emit_rpath("@executable_path/deps", &target_os);
        emit_rpath("@executable_path", &target_os);
    } else if target_os == "linux" {
        emit_rpath("$ORIGIN/deps", &target_os);
        emit_rpath("$ORIGIN", &target_os);
    }
}
fn emit_rpath(path: &str, target_os: &str) {
    if matches!(target_os, "macos" | "linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
    }
}
