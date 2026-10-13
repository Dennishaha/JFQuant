mod native_paths;
fn main() {
    println!("cargo:rerun-if-changed=native_paths.rs");
    for variable in ["LIGHTGBM_LIB_DIR", "HOME", "PATH"] {
        println!("cargo:rerun-if-env-changed={variable}");
    }
    if std::env::var_os("CARGO_FEATURE_LIGHTGBM_NATIVE").is_some() {
        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap();
        if let Some(dir) = native_paths::lightgbm_dir(&target_os) {
            println!("cargo:rustc-link-search=native={}", dir.display());
        } else {
            println!(
                "cargo:warning=LightGBM library was not found; set LIGHTGBM_LIB_DIR or install Homebrew lightgbm"
            );
        }
        println!("cargo:rustc-link-lib=dylib=_lightgbm");
    }
}
